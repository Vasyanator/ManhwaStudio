/*
FILE HEADER (cleaning/tools/watermark_library_window.rs)

Purpose:
The management screen of the watermark library — the screen that turns the on-disk entries
of `watermark_library.rs` into something a user can read and curate. It is the BODY of the
«Библиотека знаков» dock tab of the Cleaning tab, supplied by the «Удаление водяных знаков»
tool through `CleaningTool::draw_library_panel` (`watermark_removal.rs`). Every floating
surface of that program tab is a dock tab (`egui-docs/01-app-shell.md` §3.1), so this state
owns no window, reports no rect of its own, and has no close affordance: the two
«Библиотека знаков…» buttons of the tool TOGGLE it.

Main responsibilities:
- LIST mode: one card per entry — the mark composited on white, the name, the quality
  verdict with its warnings, and the two controls a card has (chapter membership and
  «Редактировать»);
- EDIT mode: one entry at a time — its name, its stored calibration samples (each with its
  own delete), the crops captured for it but not yet committed, and every per-entry action
  (add a level, export, delete);
- ask the tool to reserve the next canvas selection for the library, and fold the crop it
  captures back in;
- run every one of those off the GUI thread and poll the channel per frame.

Key structures:
- `WatermarkLibraryWindow` — the whole state, owned by `WatermarkRemovalTool`.
- `LibraryPanelMode` — which of the two screens is up.
- `LibraryPanelContext` / `LibraryPanelRequest` — what the tool hands in and what the body
  asks back; `LibraryArm` — the ENTRY ID the next canvas selection is reserved for.
- `CapturedCrop` — one canvas selection the tool already cut out and measured, with the
  background it actually carries per side; `LibraryCaptureRefusal` — why the lane refused one.
- `PendingCrop` / `CaptureRefusal` — the two kinds of row an armed selection can produce.
- `IntakeForm` — the reference-crop form (picked files plus the new entry's name).
- `LibraryEvent` — the worker protocol; `EntryAction` — what one card asked for.

Key functions:
- `poll()` (drains the workers and asks for the listing), `draw_panel_body()` (the dock tab
  body), `toggle()` (the tab's one affordance), `take_changed()` (tells the tool the on-disk
  library moved, so its own picker reloads), `accept_captured_crop()` (the armed selection
  came back), `reject_capture()` (it did not, and the reason gets a row anyway),
  `request_repaint_if_dirty()` (drained by the tool at the end of its frame).

Notes:
- `poll()` and `draw_panel_body()` are deliberately separate. The dock body does not run
  while the panel is hidden, so polling from it would strand a worker that finished after
  the user toggled the panel shut; `poll()` therefore runs from the tool's per-frame
  `draw_overlay_ui`, whatever the panel's visibility.
- The dock body runs INSIDE `CanvasView::draw`, i.e. before the tool's `draw_overlay_ui`
  of the same frame, so a flag this state raises while drawing is consumed the same frame.
  That is the ONLY route by which this screen reaches the canvas or the chapter catalog.
- The QUALITY column is the point of the screen. It reports the engine's own graded verdict
  rebuilt from the stored metadata (`watermark_entry::entry_warnings`), so the wording
  cannot drift from the chapter mode's: the IMPRINT is what is measured exactly, never «c»;
  the stated ±% bounds the alpha SCALE only. An entry that rests on a HAND-STATED background
  may never be described as measured, whatever its verdict tag says — that is a correctness
  obligation, not a nicety, and `EntryWarnings::rests_on_assertion` is what decides it.
- EVERY armed selection is answered on the screen of the entry it was aimed at, whichever
  lane refused it: `accept_captured_crop` for a crop that exists, `reject_capture` for one
  that never did. Both produce the same red block, and both log. The panel-wide status is
  drawn INSIDE each screen's scroll area (`draw_status`), never as a grey line above it, and
  anything recorded outside the draw sets `dirty` so the tool can ask for the frame that
  paints it.
- «+ новый» CREATES an empty entry and opens it; it arms nothing. There is no draft screen and
  no two-crops-upfront rule on that path — the separability rule belongs to FITTING a model,
  and an entry with no samples is a state the store already expresses.
- The card icon is the mark itself composited on white (`render_mark_on_white`), not the
  first sample and not the raw template: it is the only view that shows the MARK rather than
  the page it was cut from. An entry with no model has nothing to composite, and then the
  stored `template.png` stands in — unless there is none either, which is an EMPTY entry, and
  the card then says so in words rather than inventing a picture.
- A display name is USER DATA. The editor keeps a pending copy per entry id and writes it
  back VERBATIM — no trim, no case folding, no normalization.
- Deletion is confirmed inline (a second click on an armed button) rather than through a
  native modal, which would block the GUI thread. Deleting ONE stored crop is armed the same
  way, and is not a file deletion: `watermark_entry::drop_entry_sample` refits the entry from
  the crops that remain and the whole entry is rewritten. The arm is an INDEX into the row
  list on screen, so it is dropped by everything that invalidates that list — leaving the
  screen, opening another entry, a write, or the entry vanishing from the listing. Dropping to
  one crop is allowed (the verdict degrades to `deposit_exact`); dropping the LAST one leaves a
  template-only entry, which is why its button warns differently before the second click.
- TRIMMING a footprint is an EXPLICIT per-entry action («Обрезать отпечаток»,
  `EntryAction::TrimFootprint` -> `watermark_entry::trim_entry_footprint` -> `save_entry`), not
  something that happens on load. The rewrite swaps the whole entry directory, so the pixels it
  cuts away are gone with no undo, and that has to be a decision the user takes rather than a
  side effect of opening a screen. The button is offered only for an entry that HAS a mark, the
  whole job runs on a worker (a decode per stored crop, two fits and a directory rewrite), and
  the three outcomes each get their own status line — trimmed with both sizes, already tight, or
  no mark yet. The `TooSmall` intake refusal names the action alongside its re-drag advice,
  because a crop demand several times the size of the mark is the ENTRY's fault and no drag can
  fix it at a page edge.
- A captured crop whose ring the engine refuses to call flat CANNOT be persisted: the
  on-disk sample format has no "level unknown" state. Such a crop therefore waits in
  SESSION state until the user states its background level, and is lost on restart. The
  scratch PNG behind it lives in the system temp directory and is removed when the crop is
  committed or discarded; crops a killed run left behind are swept by
  `watermark_removal::sweep_stale_scratch_crops` once per process.
- EVERY captured crop gets a ROW, including one whose ring measured flat against an existing
  entry, which is committed the moment it arrives. A row leaves this panel only when the worker
  CONFIRMS the entry was written. A refused intake deliberately keeps its scratch files, and
  rows dropped before the answer would leave those files unreachable from every screen.
- A REFUSED crop says so ON ITS ROW (`PendingRefusal`), not only on the panel's status line: the
  status line sits above the separator at the very top of the body, while the user who just
  dragged a rectangle is looking at the sample list of the entry they aimed it at. The row
  carries the engine's own reason plus, for the three refusals a different drag can fix
  (`ReferenceRefusal`), the sentence that says which drag would have worked.
- The panel owns ONE worker channel, so an intake that arrives while a listing or an icon
  batch is in flight is QUEUED (`deferred`), never refused: a canvas capture is a selection
  the user already made, and dropping it would take the selection and leak the file.
*/
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use eframe::egui;
use egui::{Color32, TextureHandle, TextureOptions};
use ms_thread as thread;
use ms_widgets::ViewportColorSelector;

use super::watermark_entry::{
    CropMargins, EntryWarnings, FootprintTrimOutcome, ReferenceIntakeRequest, ReferenceRefusal,
    drop_entry_sample, empty_entry_request, entry_warnings, render_mark_on_white,
    run_reference_intake, trim_entry_footprint,
};
use super::watermark_library::{
    ENTRY_ARCHIVE_EXTENSION, EntrySummary, StoredSampleBackground, StoredSampleOrigin,
    delete_entry, export_entry_dir, export_entry_zip, import_entry, list_entries, load_entry,
    load_entry_template, rename_entry, save_entry,
};
use crate::watermark_chapter::{ModelConditioning, SampleParams};

/// Side of one entry's card icon, points.
const PREVIEW_SIDE: f32 = 72.0;
/// Side of one calibration sample's thumbnail in the edit screen, points.
const SAMPLE_PREVIEW_SIDE: f32 = 48.0;
/// Largest side a decoded image is downscaled to before it becomes a texture, pixels.
/// A mark is small already; this only stops a 512-px one from costing four textures'
/// worth of VRAM for a 72-point thumbnail.
const PREVIEW_MAX_PX: u32 = 128;
/// Width of the name editor, points.
const NAME_EDIT_WIDTH: f32 = 240.0;
/// Fill of the button that cancels an armed canvas selection. Red because it is the only
/// control on this screen that abandons something the user started.
///
/// `pub(super)` because the CANVAS says the same thing in the same colour: the tool tints its
/// selection cursor with it while a reservation is live (`WatermarkRemovalTool::draw_cursor`),
/// and two independent reds would let the panel and the canvas drift apart.
pub(super) const ARM_CANCEL_RGB: [u8; 3] = [150, 60, 60];
/// Starting colour of a pending crop's control: a neutral mid-grey that is deliberately NOT
/// a plausible measurement, so a level left untouched is visibly not an answer.
const PENDING_LEVEL_PLACEHOLDER: Color32 = Color32::from_rgb(128, 128, 128);
/// Fewest crops the intake accepts for a brand-new entry. Mirrors the engine's own rule —
/// one background cannot tell `alpha` from `W` — and is only used to keep the draft's
/// commit button disabled until the request could possibly succeed.
const DRAFT_MIN_CROPS: usize = 2;

/// Which screen the library panel shows.
///
/// Mode is panel state, not tool state: nothing outside this file reads it, and the dock
/// body is allowed to mutate the state its own widgets own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum LibraryPanelMode {
    /// The card list.
    #[default]
    List,
    /// One stored entry: its name, its samples and its per-entry actions.
    Entry(String),
}

/// Which entry the next completed canvas selection is reserved for.
///
/// A reservation is always FOR an entry that already exists on disk. It used to have a second
/// purpose — collecting crops for an entry that did not exist yet — which is gone: «+ новый»
/// creates the entry immediately and the user then adds samples to it, so there is no longer a
/// nameless thing a crop can be aimed at. At most one reservation exists at a time (it is an
/// `Option` on the tool), so arming one entry necessarily disarms another.
pub(super) type LibraryArm = String;

/// What the library panel asked the tool to do.
///
/// Raised while the dock body draws and consumed by the tool's `draw_overlay_ui` in the same
/// frame. The panel may not touch the chapter catalog or the canvas itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LibraryPanelRequest {
    /// Put this entry into the open chapter's search list.
    SelectEntry(String),
    /// Drop the chapter mark that came from this entry, and only that one.
    UnselectEntry(String),
    /// Reserve the next completed canvas selection for this entry.
    Arm(LibraryArm),
    /// Forget the reservation.
    Disarm,
}

/// What the tool hands the dock body for one frame.
///
/// Everything here is read-only: the body reports back through [`LibraryPanelRequest`]
/// instead of reaching into the tool.
pub(super) struct LibraryPanelContext<'a> {
    /// Ring measurement tunables, already the tool's normalized ones, so the intake and the
    /// chapter mode cannot disagree about flatness.
    pub sample_params: SampleParams,
    /// Largest footprint side the chapter detector accepts, pixels.
    pub max_side: u32,
    /// Ids of the entries the open chapter is currently searching for through a mark LOADED
    /// from the library, i.e. the ones a card may take back out again.
    pub selected_entries: &'a [String],
    /// Ids of the entries a chapter-DISCOVERED mark was written to («В библиотеку»).
    ///
    /// Such an entry is already in the chapter under the mark's own kind id, so loading it
    /// would give the catalog a second kind for one physical mark — and taking it out would
    /// throw away measurements that cost a scan. The card therefore offers neither.
    pub saved_entries: &'a [String],
    /// The entry the next canvas selection is reserved for, when there is one.
    pub armed: Option<&'a str>,
    /// True while a chapter worker owns the catalog, so membership must stay read-only.
    pub chapter_busy: bool,
}

/// One canvas selection the tool already cut out of its page and measured.
///
/// GUI-free by construction: the tool's worker produces it, the panel uploads the preview.
#[derive(Debug)]
pub(super) struct CapturedCrop {
    /// Where the crop was written so the reference intake can read it back. Session scratch:
    /// the panel deletes it when the crop is committed or discarded.
    pub path: PathBuf,
    /// Thumbnail of the crop, ready to upload.
    pub preview: egui::ColorImage,
    /// The measured background level, present only when the ring measurement ACCEPTED the
    /// crop. `None` means the engine refused to call the ring flat, and the crop cannot be
    /// persisted until the user states a level.
    pub measured: Option<[f32; 3]>,
    /// Background actually present on each side of the mark inside `path`.
    ///
    /// Not always symmetric: a mark stamped against the page border keeps only what the page
    /// could give on that side. The intake needs it to derive the footprint that was cut
    /// instead of assuming an inset the crop does not have.
    pub margins: CropMargins,
}

/// Why the capture lane refused one armed canvas selection.
///
/// The same shape a refused INTAKE has, and for the same reason: the message says what went
/// wrong and the tag says what the user can do about it, so one refusal row can carry both
/// whichever lane produced it. Before this existed, a capture-lane refusal had nowhere to land
/// but the panel's status line — no row, no thumbnail, and nothing on the screen the drag was
/// aimed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LibraryCaptureRefusal {
    /// Already-localized and user-facing.
    pub message: String,
    pub refusal: ReferenceRefusal,
}

impl LibraryCaptureRefusal {
    /// A refusal a different drag could fix, carrying the tag that says which one.
    #[must_use]
    pub(super) fn tagged(message: String, refusal: ReferenceRefusal) -> Self {
        Self { message, refusal }
    }

    /// A refusal no gesture fixes — a page that will not decode, a file that cannot be written.
    #[must_use]
    pub(super) fn untagged(message: String) -> Self {
        Self {
            message,
            refusal: ReferenceRefusal::Other,
        }
    }
}

/// What a file picker was opened for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerPurpose {
    /// Image files for a new entry built from reference crops.
    ReferenceCrops,
    /// Image files that add one more level to an existing entry.
    ImproveEntry(String),
    /// A `.zip` archive to import.
    ImportArchive,
    /// A directory holding an unpacked entry.
    ImportFolder,
    /// Where to write one entry's archive.
    ExportArchive(String),
    /// Where to copy one entry as a folder.
    ExportFolder(String),
}

/// One entry's icon, and the entry revision it was rendered from.
///
/// Keyed by revision because an entry that gains a sample gets a different mark: a texture
/// cache that only evicted on delete would keep showing the old one forever.
struct EntryIcon {
    revision: u64,
    texture: TextureHandle,
}

/// One rendered icon as it crosses the channel.
struct IconRender {
    id: String,
    revision: u64,
    /// `None` when the render failed; `error` then says why.
    image: Option<egui::ColorImage>,
    /// Already-localized failure message.
    error: Option<String>,
}

/// One stored calibration sample of the entry being edited.
struct SampleRow {
    /// Entry-relative file name, as `entry.json` records it. Not localized.
    file: String,
    origin: StoredSampleOrigin,
    background: StoredSampleBackground,
    /// Thumbnail waiting to be uploaded; `None` once `texture` holds it.
    image: Option<egui::ColorImage>,
    texture: Option<TextureHandle>,
}

/// The stored samples of one entry, and the entry revision they were read at.
struct EntrySamples {
    entry_id: String,
    revision: u64,
    rows: Vec<SampleRow>,
}

/// Why the intake refused the crop of one pending row, as the row shows it.
///
/// Kept ON THE ROW rather than only on the panel's status line: the status line sits at the
/// very top of the panel body, above the separator, while the user who just dragged a
/// rectangle is looking at the sample list of the entry they aimed it at.
struct PendingRefusal {
    /// The engine's own already-localized reason.
    reason: String,
    /// What to do about it, when the refusal is one a different drag can fix. `None` for a
    /// refusal with no advice to give (a decode failure, a file that vanished).
    advice: Option<String>,
}

/// One armed canvas selection the CAPTURE lane refused, as a row on the entry it was aimed at.
///
/// It exists because the "every captured crop gets a row" guarantee used to start one step too
/// late — at [`WatermarkLibraryWindow::accept_captured_crop`], which only ever sees a crop that
/// was successfully cut. Everything that failed before that (a busy lane, a page that is not in
/// the chapter, a selection the engine could not measure, a scratch file that would not write)
/// produced no row at all and only a grey line at the top of the panel, far from the list the
/// user was watching. A refusal row carries no crop and no scratch file — there is nothing to
/// commit — only the reason, the advice and a button to dismiss it.
struct CaptureRefusal {
    /// Entry the drag was aimed at.
    target: String,
    /// The engine's own already-localized reason.
    reason: String,
    /// What to do about it, when a different drag would have worked.
    advice: Option<String>,
}

/// A crop captured from the canvas that is not in the library yet.
///
/// It exists because the on-disk sample format has no "level unknown" state: a crop whose
/// ring the engine refused cannot be written at all until the user states its background.
/// Session-only by consequence — a restart loses it, which is honest, because nothing about
/// it was ever measured.
struct PendingCrop {
    /// Entry the crop will be added to. Always a real entry: «+ новый» creates one before any
    /// crop is taken, so a crop is never aimed at something that does not exist.
    target: String,
    /// Session scratch PNG the intake will read.
    path: PathBuf,
    /// Background present on each side of the mark inside that PNG, so the intake derives the
    /// footprint that was cut rather than a symmetric inset the crop may not have.
    margins: CropMargins,
    image: Option<egui::ColorImage>,
    texture: Option<TextureHandle>,
    /// The background level that will be handed to the intake for this crop.
    level: Color32,
    /// True once the level is something other than the control's starting guess: either the
    /// engine MEASURED it, or the user stated it. While it is false the crop may not be
    /// committed — writing the starting guess would record a claim nobody made.
    stated: bool,
    /// The colour control's own state (popup and eyedropper), one per row.
    picker: ViewportColorSelector,
    /// Set when an intake this crop was handed to REFUSED it. The row keeps its scratch file
    /// and its discard button, so the refusal is something the user can read, act on and
    /// dismiss instead of a crop that silently never arrived.
    refusal: Option<PendingRefusal>,
}

/// Messages a library worker sends back.
enum LibraryEvent {
    List(Vec<EntrySummary>),
    Icons(Vec<IconRender>),
    /// The stored samples of the entry the edit screen is showing.
    Samples(Box<EntrySamples>),
    /// A mutation finished; the message is already localized and the listing is now stale.
    Changed(String),
    /// An entry was CREATED. Same as `Changed` plus the id, because the panel walks straight
    /// onto the new entry's own screen — which it cannot do from a status string.
    Created { entry_id: String, status: String },
    /// A step failed. `message` is already localized; `refusal` classifies an INTAKE failure so
    /// the crop rows it was about can carry advice the user can act on, and is
    /// [`ReferenceRefusal::Other`] for every other job.
    Failed {
        message: String,
        refusal: ReferenceRefusal,
    },
}

impl LibraryEvent {
    /// A failure carrying no actionable classification — everything that is not a reference
    /// intake (a listing, an icon batch, a rename, a delete, an export, an import).
    fn failed(message: String) -> Self {
        Self::Failed {
            message,
            refusal: ReferenceRefusal::Other,
        }
    }
}

/// One "build or extend an entry from these crops" job, as the panel starts it.
///
/// A struct rather than seven parameters: every caller fills all of them, and three of them
/// are `Vec`s whose order is load-bearing (`manual_levels[i]` belongs to `files[i]`), which is
/// exactly the mistake a positional argument list invites.
struct IntakeJob {
    /// Crops in the order the intake must read them. The first one defines the footprint of a
    /// NEW entry.
    files: Vec<PathBuf>,
    /// Levels asserted per crop, index-aligned with `files`. Empty = nothing asserted.
    manual_levels: Vec<Option<[f32; 3]>>,
    /// Background each crop carries per side, index-aligned with `files`. Empty (or a `None`
    /// slot) = the intake's ordinary symmetric rule, which is what a file the USER picked gets.
    crop_margins: Vec<Option<CropMargins>>,
    /// Display name of a NEW entry, stored VERBATIM. Ignored when `target` is set.
    name: String,
    /// `Some(entry_id)` extends that entry instead of creating one.
    target: Option<String>,
    sample_params: SampleParams,
    max_side: u32,
    /// Session scratch files to delete once the entry is written. Only the panel's own copies
    /// of canvas crops ever appear here — a file the USER picked is never deleted.
    scratch: Vec<PathBuf>,
}

/// The reference-crop form.
#[derive(Debug, Default)]
struct IntakeForm {
    /// Shown only while the user is filling it in.
    open: bool,
    /// Display name of the entry the crops will create. Stored VERBATIM.
    name: String,
    /// Files picked so far, in pick order. The first one defines the footprint.
    files: Vec<PathBuf>,
    /// `Some(entry_id)` adds these crops to that entry instead of creating a new one.
    target: Option<String>,
}

impl IntakeForm {
    /// Clears the form back to "nothing picked".
    fn reset(&mut self) {
        self.open = false;
        self.name.clear();
        self.files.clear();
        self.target = None;
    }
}

/// The library management screen: the state behind the «Библиотека знаков» dock tab.
#[derive(Default)]
pub(super) struct WatermarkLibraryWindow {
    open: bool,
    mode: LibraryPanelMode,
    entries: Vec<EntrySummary>,
    /// Cleared until the first listing answered, so the window fills itself once.
    listed: bool,
    /// Pending name edits keyed by entry id. Only entries the user typed into appear here,
    /// so a listing refresh never fights the text cursor.
    edits: HashMap<String, String>,
    /// Rendered card icons, keyed by entry id and validated against the entry's revision.
    icons: HashMap<String, EntryIcon>,
    /// Revision an icon render was already attempted at, keyed by entry id. Without it an
    /// entry whose icon cannot be produced would be retried — and a job started — every
    /// frame; with it, a CHANGED entry is still retried exactly once.
    icons_tried: HashMap<String, u64>,
    /// The stored samples of the entry the edit screen shows, when they are loaded.
    samples: Option<EntrySamples>,
    /// Entry id and revision a sample load was already asked for.
    samples_tried: Option<(String, u64)>,
    /// Crops captured from the canvas that are not in the library yet.
    pending: Vec<PendingCrop>,
    /// Scratch paths of the pending crops the intake NOW IN FLIGHT was given.
    ///
    /// A crop leaves `pending` only once the worker answered: an intake that is refused keeps
    /// its scratch files on purpose (`run_intake`), and they are the user's only copy of that
    /// selection — dropping the rows first would leave those files unreachable from every
    /// screen. Empty whenever the running job is not an intake.
    committing: Vec<PathBuf>,
    /// Intake jobs that arrived while the panel's single channel was busy.
    ///
    /// A capture must never be lost. The panel runs ONE job at a time, so a crop that comes
    /// back during a listing or an icon batch cannot be written straight away — it waits here
    /// and is started by `poll` the moment the channel frees, instead of being dropped with
    /// its scratch file leaked.
    deferred: Vec<IntakeJob>,
    /// Capture-lane refusals waiting to be read, one row each on the entry they were aimed at.
    capture_refusals: Vec<CaptureRefusal>,
    confirm_delete: Option<String>,
    /// Entry id and row index of the stored sample whose delete button is ARMED, i.e. waiting
    /// for its second click. Two-click for the same reason the entry's own delete is: the crop
    /// may be the only measurement of that background, and deleting it refits the whole entry.
    /// Keyed by index into the CURRENT row list, so it is dropped whenever that list is
    /// invalidated — an index that outlived its list would arm a different crop.
    confirm_delete_sample: Option<(String, usize)>,
    intake: IntakeForm,
    rx: Option<Receiver<LibraryEvent>>,
    status: Option<String>,
    picker_rx: Option<Receiver<Option<Vec<PathBuf>>>>,
    picker: Option<PickerPurpose>,
    /// Set whenever the on-disk library changed, so the chapter mode's own picker reloads.
    changed: bool,
    /// Set whenever something the panel SHOWS changed outside the draw — a status, a refusal
    /// row, a mode walk. Drained by `request_repaint_if_dirty`.
    dirty: bool,
}

impl WatermarkLibraryWindow {
    /// Flips the «Библиотека знаков» panel between shown and hidden.
    ///
    /// Opening it marks the listing stale, so the panel always shows the current disk. This
    /// is the tab's ONLY affordance — a dock tab has no close button of its own — and both
    /// «Библиотека знаков…» buttons of the tool call exactly this.
    pub(super) fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.listed = false;
        }
    }

    /// True while the library panel is shown.
    ///
    /// The ONE source of truth for the dock tab's `.visible(..)`; the tab must never keep a
    /// second copy of it.
    #[must_use]
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Takes the "the library changed on disk" flag, clearing it.
    ///
    /// The chapter mode keeps its own copy of the entry list; this is how it learns that
    /// copy is stale without the two states having to be shared.
    pub(super) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// True while this frame's primary click belongs to a pending crop's colour control.
    ///
    /// The tool refuses to begin a canvas selection while this holds: the eyedropper's click
    /// lands on the viewport, and the canvas must not read it as the start of a drag. See
    /// [`eyedropper_owns_click`] for why one predicate is not enough.
    #[must_use]
    pub(super) fn eyedropper_owns_primary_click(&self) -> bool {
        self.pending.iter().any(|crop| {
            eyedropper_owns_click(
                crop.picker.eyedropper_active(),
                crop.picker.primary_click_consumed_this_frame(),
            )
        })
    }

    /// True while `arm` is still drawn on the screen the user is looking at.
    ///
    /// A reservation can only be called off from the red «Отменить выделение» that replaces
    /// the arming button, and that button lives on ONE screen: the own screen of the entry the
    /// reservation is for. A reservation whose screen is gone — the panel was hidden, the entry
    /// was deleted, the user walked back to the card list — is one the user can no longer see or
    /// cancel, yet it would still claim the next canvas selection. The tool asks this every
    /// frame and drops the reservation when the answer is `false`.
    #[must_use]
    pub(super) fn arm_survives(&self, arm: &str) -> bool {
        if !self.open {
            return false;
        }
        match &self.mode {
            // The entry screen early-returns when the listing does not carry the entry, so an
            // entry deleted under an armed reservation draws no cancel either.
            LibraryPanelMode::Entry(shown) => {
                arm == shown && self.entries.iter().any(|entry| &entry.id == shown)
            }
            LibraryPanelMode::List => false,
        }
    }

    /// Read-only test accessor for the panel's status line.
    ///
    /// The panel's own body is what shows it in the product, so nothing outside a test needs
    /// to read it; it exists so a sibling module's test can assert that a refusal REACHED the
    /// user instead of only asserting that the code path ran.
    #[cfg(test)]
    #[must_use]
    pub(super) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Shows one already-localized message on the panel's status line.
    ///
    /// The status line is the panel-wide channel, drawn on the screen the user is actually on
    /// (see `draw_status`). It is NOT the channel for a refusal aimed at one entry: that gets a
    /// row through [`WatermarkLibraryWindow::reject_capture`], because a reason has to appear
    /// where the drag was aimed.
    pub(super) fn set_status(&mut self, status: String) {
        self.status = Some(status);
        self.dirty = true;
    }

    /// Asks for one more frame when something the panel shows changed outside the draw.
    ///
    /// The dock body runs inside `CanvasView::draw`, which is EARLIER in the frame than the
    /// tool's `draw_overlay_ui`: a status or a refusal row recorded there is therefore already
    /// too late for the frame that recorded it. With no repaint it waits for the next input
    /// event, and after a finished drag there may not be one — the user releases the mouse and
    /// the application goes quiet holding a message it has never painted. That is precisely how
    /// a refused capture became invisible, so this is a correctness call, not a smoothness one.
    pub(super) fn request_repaint_if_dirty(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.dirty) {
            ctx.request_repaint();
        }
    }

    /// Records a CAPTURE-lane refusal as a row on the entry the drag was aimed at.
    ///
    /// This is the early half of the "every armed selection is answered visibly" guarantee. The
    /// late half lives in `accept_captured_crop` / `mark_committing_refused` and can only speak
    /// for a crop that exists; everything that fails before a crop exists arrives here. Both
    /// halves produce the same red block with the same reason and the same gesture advice, so
    /// the user cannot tell — and does not need to tell — which lane refused them.
    ///
    /// The panel is also walked onto the entry's own screen, for the same reason the accepted
    /// path walks there: the row is only readable on the screen that draws it.
    ///
    /// It logs as well. A lane that reported nothing to the runtime log is why this defect could
    /// not be diagnosed from a log the user sent, which is itself a defect and not a detail.
    pub(super) fn reject_capture(&mut self, entry_id: &str, refusal: &LibraryCaptureRefusal) {
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library capture for entry {entry_id} refused ({:?}): {}",
            refusal.refusal, refusal.message
        ));
        self.mode = LibraryPanelMode::Entry(entry_id.to_string());
        self.dirty = true;
        self.capture_refusals.push(CaptureRefusal {
            target: entry_id.to_string(),
            reason: refusal.message.clone(),
            advice: refusal_advice(refusal.refusal),
        });
    }

    /// Read-only test accessor for the capture-refusal rows.
    ///
    /// The product reads them off the panel body; this exists so a sibling module's test can
    /// assert a refusal produced a ROW rather than only a status line.
    #[cfg(test)]
    #[must_use]
    pub(super) fn capture_refusal_reasons(&self, target: &str) -> Vec<&str> {
        self.capture_refusals
            .iter()
            .filter(|row| row.target == target)
            .map(|row| row.reason.as_str())
            .collect()
    }

    /// True while a worker or a file dialog owns the library state.
    fn busy(&self) -> bool {
        self.rx.is_some() || self.picker_rx.is_some()
    }

    /// Starts a worker, refusing a second job while one is in flight: two jobs would leave
    /// the list drawn from two different states of the disk.
    fn start_job<F>(&mut self, work: F)
    where
        F: FnOnce(&Sender<LibraryEvent>) + Send + 'static,
    {
        if self.rx.is_some() {
            self.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return;
        }
        // Only an intake owns pending rows, and it records them again right after this call:
        // clearing here keeps a rename or an export from ever being mistaken for the commit
        // whose crops are waiting.
        self.committing.clear();
        let (tx, rx) = mpsc::channel::<LibraryEvent>();
        self.rx = Some(rx);
        thread::spawn(move || work(&tx));
    }

    /// Reloads the entry list from disk.
    fn request_list(&mut self) {
        self.listed = true;
        self.start_job(|tx| {
            let _ = tx.send(LibraryEvent::List(list_entries()));
        });
    }

    /// Renders the card icon of every listed entry whose icon is missing or stale.
    ///
    /// ONE job for the whole batch. The render is two small PNG decodes per entry and the
    /// panel's channel holds a single job at a time, so a job per card would both starve
    /// rename and delete and spawn a thread per row on the first frame.
    fn request_icons(&mut self) {
        let wanted: Vec<(String, u64, bool)> = self
            .entries
            .iter()
            .filter(|entry| self.icons_tried.get(&entry.id) != Some(&entry.updated_unix))
            .map(|entry| (entry.id.clone(), entry.updated_unix, entry.has_template))
            .collect();
        if wanted.is_empty() {
            return;
        }
        for (id, revision, _) in &wanted {
            self.icons_tried.insert(id.clone(), *revision);
        }
        self.start_job(move |tx| {
            let renders = wanted
                .into_iter()
                .map(|(id, revision, has_template)| render_entry_icon(id, revision, has_template))
                .collect();
            let _ = tx.send(LibraryEvent::Icons(renders));
        });
    }

    /// Reads the stored calibration samples of the entry the edit screen is showing.
    fn request_samples(&mut self, entry_id: &str, revision: u64) {
        self.samples_tried = Some((entry_id.to_string(), revision));
        let entry_id = entry_id.to_string();
        self.start_job(move |tx| {
            let event = match load_entry(&entry_id) {
                Ok(entry) => {
                    let rows = entry
                        .samples
                        .iter()
                        .zip(entry.meta.samples.iter())
                        .map(|(sample, stored)| SampleRow {
                            file: stored.file.clone(),
                            origin: sample.origin,
                            background: sample.background,
                            image: Some(thumbnail(&sample.image)),
                            texture: None,
                        })
                        .collect();
                    LibraryEvent::Samples(Box::new(EntrySamples {
                        entry_id,
                        revision,
                        rows,
                    }))
                }
                Err(err) => LibraryEvent::failed(err),
            };
            let _ = tx.send(event);
        });
    }

    /// Drains the worker channel and folds every finished step into the state.
    fn poll_job(&mut self, ctx: &egui::Context) {
        loop {
            let event = {
                let Some(rx) = self.rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(LibraryEvent::List(entries)) => {
                    self.fold_listing(entries);
                    self.finish_job();
                }
                Ok(LibraryEvent::Icons(renders)) => {
                    for render in renders {
                        match render.image {
                            Some(image) => {
                                let texture = ctx.load_texture(
                                    format!("cleaning-watermark-library-{}", render.id),
                                    image,
                                    TextureOptions::NEAREST,
                                );
                                self.icons.insert(
                                    render.id,
                                    EntryIcon {
                                        revision: render.revision,
                                        texture,
                                    },
                                );
                            }
                            // A render that failed is REPORTED, not swallowed: the message is
                            // already user-facing (an unreadable entry, a compositing law this
                            // build does not implement) and the card would otherwise just stay
                            // blank with no reason given.
                            None => {
                                if let Some(err) = render.error {
                                    self.status = Some(err);
                                }
                            }
                        }
                    }
                    self.finish_job();
                }
                Ok(LibraryEvent::Samples(samples)) => {
                    self.samples = Some(*samples);
                    self.finish_job();
                }
                // A CREATED entry walks the panel onto its own screen: the user asked for a
                // new mark in order to fill it, and the screen that takes samples is the one
                // they need. Everything else is the same as `Changed`.
                Ok(LibraryEvent::Created { entry_id, status }) => {
                    self.mode = LibraryPanelMode::Entry(entry_id);
                    self.status = Some(status);
                    self.changed = true;
                    self.listed = false;
                    self.samples = None;
                    self.samples_tried = None;
                    self.confirm_delete_sample = None;
                    self.drop_committed_pending();
                    self.finish_job();
                }
                Ok(LibraryEvent::Changed(status)) => {
                    self.status = Some(status);
                    self.changed = true;
                    self.listed = false;
                    // The entry on screen moved, so its samples must be read again — and a row
                    // index armed against the OLD list would point at a different crop in the
                    // new one.
                    self.samples = None;
                    self.samples_tried = None;
                    self.confirm_delete_sample = None;
                    self.drop_committed_pending();
                    self.finish_job();
                }
                Ok(LibraryEvent::Failed { message, refusal }) => {
                    self.status = Some(tf!(
                        "cleaning.mask_editor.processing_error",
                        err = message.clone()
                    ));
                    // The crops the refused intake was given stay exactly where they were: the
                    // worker kept their scratch files, so the rows are still backed by a file
                    // the user can commit again after fixing the level — and the rows now SAY
                    // why, because the status line above lives at the top of the panel, far
                    // from the sample list the user is watching.
                    self.mark_committing_refused(&message, refusal);
                    self.finish_job();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.status =
                        Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                    // Nothing confirmed the entry was written, so the rows stay: a crop shown
                    // twice is recoverable, a crop silently gone is not.
                    self.committing.clear();
                    self.finish_job();
                    return;
                }
            }
        }
    }

    /// Replaces the entry list and drops everything that described entries which are gone.
    ///
    /// Split out of `poll_job` so the mode can be tested without a worker: an entry deleted
    /// while its edit screen is up must send the panel back to the list, and an entry that
    /// survived the refresh must NOT.
    fn fold_listing(&mut self, entries: Vec<EntrySummary>) {
        let alive = |id: &String| entries.iter().any(|entry| &entry.id == id);
        // Drop the textures and pending edits of entries that no longer exist, or the maps
        // would grow one dead item per delete.
        self.icons.retain(|id, _| alive(id));
        self.icons_tried.retain(|id, _| alive(id));
        self.edits.retain(|id, _| alive(id));
        if self
            .confirm_delete_sample
            .as_ref()
            .is_some_and(|(id, _)| !alive(id))
        {
            self.confirm_delete_sample = None;
        }
        if let LibraryPanelMode::Entry(entry_id) = &self.mode
            && !alive(entry_id)
        {
            self.mode = LibraryPanelMode::List;
        }
        self.entries = entries;
    }

    /// Clears the in-flight marker once a job reported its last event.
    fn finish_job(&mut self) {
        self.rx = None;
    }

    /// Marks the rows of the intake that just FAILED with the reason it gave, and releases
    /// them for another attempt.
    ///
    /// Replaces the bare `committing.clear()` this used to be: clearing alone left the rows on
    /// screen looking exactly like rows nobody had tried yet, which is how a refused capture
    /// became invisible. A row that carries no reason is no better than no row at all.
    ///
    /// Does nothing when the failed job owned no rows (a listing, an icon batch, a rename).
    fn mark_committing_refused(&mut self, reason: &str, refusal: ReferenceRefusal) {
        let committing = std::mem::take(&mut self.committing);
        if committing.is_empty() {
            return;
        }
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library intake refused {} crop(s) ({refusal:?}): {reason}",
            committing.len()
        ));
        let advice = refusal_advice(refusal);
        for crop in self
            .pending
            .iter_mut()
            .filter(|crop| committing.contains(&crop.path))
        {
            crop.refusal = Some(PendingRefusal {
                reason: reason.to_string(),
                advice: advice.clone(),
            });
        }
    }

    /// Drops the pending rows an intake has now WRITTEN, and only those.
    ///
    /// The worker deleted their scratch files itself, so nothing is left to clean up here.
    fn drop_committed_pending(&mut self) {
        if self.committing.is_empty() {
            return;
        }
        let committed = std::mem::take(&mut self.committing);
        self.pending.retain(|crop| !committed.contains(&crop.path));
    }

    /// Folds a finished file pick into the form or starts the job it was picked for.
    fn poll_picker(&mut self) {
        let Some(rx) = self.picker_rx.as_ref() else {
            return;
        };
        let picked = match rx.try_recv() {
            Ok(picked) => picked,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.picker_rx = None;
        let Some(purpose) = self.picker.take() else {
            return;
        };
        let Some(paths) = picked.filter(|paths| !paths.is_empty()) else {
            return;
        };
        match purpose {
            PickerPurpose::ReferenceCrops => {
                self.intake.open = true;
                self.intake.target = None;
                self.intake.files = paths;
            }
            PickerPurpose::ImproveEntry(entry_id) => {
                self.intake.open = true;
                self.intake.target = Some(entry_id);
                self.intake.files = paths;
            }
            PickerPurpose::ImportArchive | PickerPurpose::ImportFolder => {
                let source = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match import_entry(&source) {
                        Ok(entry_id) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_imported_status",
                            id = entry_id
                        )),
                        Err(err) => LibraryEvent::failed(err),
                    });
                });
            }
            PickerPurpose::ExportArchive(entry_id) => {
                let dest = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match export_entry_zip(&entry_id, &dest) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_exported_status",
                            path = dest.display()
                        )),
                        Err(err) => LibraryEvent::failed(err),
                    });
                });
            }
            PickerPurpose::ExportFolder(entry_id) => {
                let parent = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match export_entry_dir(&entry_id, &parent) {
                        Ok(path) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_exported_status",
                            path = path.display()
                        )),
                        Err(err) => LibraryEvent::failed(err),
                    });
                });
            }
        }
    }

    /// Opens a native file dialog on a worker thread for `purpose`.
    fn start_picker(&mut self, purpose: PickerPurpose) {
        if self.picker_rx.is_some() {
            return;
        }
        self.picker_rx = Some(spawn_picker(&purpose));
        self.picker = Some(purpose);
    }

    /// Runs the reference-crop intake and writes the resulting entry.
    fn start_intake(&mut self, sample_params: SampleParams, max_side: u32) {
        let files = self.intake.files.clone();
        let name = self.intake.name.clone();
        let target = self.intake.target.clone();
        self.intake.reset();
        self.run_intake(IntakeJob {
            files,
            // This form is fed by the file picker, which offers no colour control, so it
            // asserts no level and the intake keeps its measurement-only behaviour.
            manual_levels: Vec::new(),
            // Nor does a picked file say where the mark sits inside it: the intake searches for
            // it under its ordinary symmetric rule, which is the only assumption available.
            crop_margins: Vec::new(),
            name,
            target,
            sample_params,
            max_side,
            scratch: Vec::new(),
        });
    }

    /// Starts the intake job for an explicit crop list, or QUEUES it when the channel is busy.
    ///
    /// Queueing is the whole point: this panel owns one worker channel, and a crop the canvas
    /// captured while a listing or an icon batch was in flight has nowhere else to live — its
    /// scratch file is the user's only copy of that selection. `poll` starts the queued job as
    /// soon as the channel frees.
    fn run_intake(&mut self, request: IntakeJob) {
        if self.rx.is_some() {
            self.status = Some(
                t!("cleaning.tools.watermark.chapter.library_intake_queued_status").to_string(),
            );
            self.deferred.push(request);
            return;
        }
        // Recorded BEFORE the job starts: the rows behind these files stay on screen until the
        // worker answers, and this is what says which ones the answer is about.
        let committing = request.scratch.clone();
        let IntakeJob {
            files,
            manual_levels,
            crop_margins,
            name,
            target,
            sample_params,
            max_side,
            scratch,
        } = request;
        self.start_job(move |tx| {
            // Loading the base entry is I/O and belongs on this thread, not on the GUI one.
            let base = match target.as_deref().map(load_entry) {
                Some(Ok(entry)) => Some(entry),
                Some(Err(err)) => {
                    let _ = tx.send(LibraryEvent::failed(err));
                    return;
                }
                None => None,
            };
            let outcome = run_reference_intake(ReferenceIntakeRequest {
                files,
                manual_levels,
                crop_margins,
                sample_params,
                base,
                name,
                max_side,
            });
            let event = match outcome.and_then(|outcome| {
                let levels = outcome
                    .conditioning
                    .levels()
                    .iter()
                    .map(|level| format!("{level:.0}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let crops = outcome.reports.len();
                // An entry whose stored footprint was re-derived from its own mark so these crops
                // could fit changed SHAPE, and the user must be told in the same line that says it
                // was written — not left to notice it on the card.
                let trimmed = outcome.trimmed;
                // `save_entry` fails for I/O reasons, which no crop geometry can fix: it
                // becomes an UNTAGGED refusal, and the row therefore carries the reason with
                // no advice under it.
                save_entry(&outcome.request).map_err(Into::into).map(|entry_id| {
                    let saved = tf!(
                        "cleaning.tools.watermark.chapter.reference_saved_status",
                        id = entry_id,
                        crops = crops,
                        levels = levels
                    );
                    match trimmed {
                        Some((before, after)) => format!(
                            "{saved} {}",
                            tf!(
                                "cleaning.tools.watermark.chapter.library_trimmed_status",
                                before_width = before.0,
                                before_height = before.1,
                                width = after.0,
                                height = after.1
                            )
                        ),
                        None => saved,
                    }
                })
            }) {
                Ok(status) => {
                    // The crop now lives inside the entry; the scratch copy is redundant.
                    for path in &scratch {
                        remove_scratch_crop(path);
                    }
                    LibraryEvent::Changed(status)
                }
                // A refused intake keeps its scratch files: the crop is still the user's only
                // copy of that selection, and the panel still shows its row — `committing` is
                // what keeps that promise, by holding the rows until this answer arrives. That
                // used to be FALSE for a crop committed straight away by `accept_captured_crop`
                // (it pushed no row at all); it now pushes one for every crop, so the promise
                // holds on both paths. The refusal TAG rides along so the row can also say what
                // to do about it.
                Err(err) => LibraryEvent::Failed {
                    message: err.message,
                    refusal: err.refusal,
                },
            };
            let _ = tx.send(event);
        });
        self.committing = committing;
    }

    /// Folds one crop the tool cut out of the canvas into the panel.
    ///
    /// EVERY crop gets a row, whatever happens to it next. A crop whose ring MEASURED flat and
    /// which belongs to an existing entry is ALSO committed straight away, because nothing is
    /// left for the user to state — but the row stays until the worker confirms the entry was
    /// written. That is what makes a refusal visible: the intake can still refuse such a crop
    /// (too small for the entry's stored footprint, not aligned with it, on a background level
    /// the entry already has), and until the row existed that refusal produced no row, no
    /// thumbnail and no sample — only a grey line at the top of the panel, far from the list
    /// the user was watching.
    ///
    /// Every other crop simply waits in session state — a draft needs a second background
    /// before the engine can separate the model, and a crop with no measurement needs a level
    /// before it can be written at all.
    ///
    /// "Straight away" survives a busy channel: `run_intake` QUEUES the job when a listing or
    /// an icon batch is in flight rather than refusing it, because a capture that is dropped
    /// takes the user's selection and leaks its scratch file with it.
    pub(super) fn accept_captured_crop(
        &mut self,
        crop: CapturedCrop,
        arm: &str,
        sample_params: SampleParams,
        max_side: u32,
    ) {
        let target = arm.to_string();
        self.mode = LibraryPanelMode::Entry(target.clone());
        self.dirty = true;
        let path = crop.path.clone();
        let margins = crop.margins;
        self.pending.push(PendingCrop {
            target: target.clone(),
            path: crop.path,
            margins,
            image: Some(crop.preview),
            texture: None,
            // Mid-grey is the control's neutral START, not an answer: `stated` is what says
            // whether the level means anything yet.
            level: crop
                .measured
                .map_or(PENDING_LEVEL_PLACEHOLDER, level_to_color),
            stated: crop.measured.is_some(),
            picker: ViewportColorSelector::default(),
            refusal: None,
        });
        // Nothing is left for the user to state, so the commit does not wait for a click. The
        // row above is what the answer lands on: `Changed` drops it, `Failed` writes the
        // reason onto it.
        if let Some(level) = crop.measured {
            self.run_intake(IntakeJob {
                files: vec![path.clone()],
                manual_levels: vec![Some(level)],
                crop_margins: vec![Some(margins)],
                name: String::new(),
                target: Some(target),
                sample_params,
                max_side,
                scratch: vec![path],
            });
        }
    }

    /// True when every crop captured for `target` carries a level that may be committed.
    ///
    /// A crop whose level is still the control's starting guess blocks the whole commit: the
    /// intake would record that guess as the user's claim about the background, and an entry
    /// built on it would be wrong in a way nothing later could detect.
    fn pending_ready(&self, target: &str) -> bool {
        let mut any = false;
        for crop in self.pending.iter().filter(|crop| crop.target == target) {
            any = true;
            if !crop.stated {
                return false;
            }
        }
        any
    }

    /// Drives the background jobs and keeps the listing fresh. Call once per frame while the
    /// tool is active, WHATEVER the panel's visibility.
    ///
    /// Draining the channel may not be tied to the dock body: a hidden panel draws nothing, so
    /// a worker that finishes after the user toggled the panel shut would never be collected —
    /// the entry would silently not appear and `take_changed` would never fire. The listing,
    /// the icons and the samples are only ASKED for while the panel is open, because nothing
    /// reads them otherwise.
    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        self.poll_job(ctx);
        self.poll_picker();
        self.upload_pending_textures(ctx);
        // A queued intake outranks the listing and the icons, and runs whatever the panel's
        // visibility: it is a capture the user already made, and the crop behind it exists
        // only as a scratch file until it is written.
        if self.rx.is_none() && !self.deferred.is_empty() {
            let job = self.deferred.remove(0);
            self.run_intake(job);
        }
        if self.open {
            if !self.listed && self.rx.is_none() {
                self.request_list();
            }
            if self.rx.is_none() {
                self.request_icons();
            }
            if self.rx.is_none()
                && let LibraryPanelMode::Entry(entry_id) = self.mode.clone()
                && let Some(revision) = self.entry_revision(&entry_id)
                && self.samples_tried.as_ref() != Some(&(entry_id.clone(), revision))
            {
                self.request_samples(&entry_id, revision);
            }
        }
        if self.busy() {
            ctx.request_repaint();
        }
    }

    /// Uploads the thumbnails that arrived from a worker, once each.
    ///
    /// Texture upload needs the `Context` and therefore the GUI thread; doing it here rather
    /// than during the draw keeps it off the per-frame path, because the source image is
    /// dropped as soon as it becomes a texture.
    fn upload_pending_textures(&mut self, ctx: &egui::Context) {
        for (index, crop) in self.pending.iter_mut().enumerate() {
            if let Some(image) = crop.image.take() {
                crop.texture = Some(ctx.load_texture(
                    format!("cleaning-watermark-library-pending-{index}"),
                    image,
                    TextureOptions::NEAREST,
                ));
            }
        }
        if let Some(samples) = self.samples.as_mut() {
            for (index, row) in samples.rows.iter_mut().enumerate() {
                if let Some(image) = row.image.take() {
                    row.texture = Some(ctx.load_texture(
                        format!(
                            "cleaning-watermark-library-sample-{}-{index}",
                            samples.entry_id
                        ),
                        image,
                        TextureOptions::NEAREST,
                    ));
                }
            }
        }
    }

    /// `updated_unix` of one listed entry, or `None` when it is not in the listing.
    fn entry_revision(&self, entry_id: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .map(|entry| entry.updated_unix)
    }

    /// Draws the «Библиотека знаков» dock tab body and returns what it asks the tool to do.
    ///
    /// Runs inside `CanvasView::draw` and mutates only this state, per the dock body rule;
    /// anything touching the chapter catalog or the canvas leaves as a request instead.
    pub(super) fn draw_panel_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &LibraryPanelContext<'_>,
    ) -> Option<LibraryPanelRequest> {
        let busy = self.busy();
        let mut request = None;
        self.draw_header(ui, busy);
        ui.small(t!("cleaning.tools.watermark.chapter.reference_help_hint"));
        self.draw_intake(ui, ctx.sample_params, ctx.max_side);
        ui.separator();
        match self.mode.clone() {
            LibraryPanelMode::List => self.draw_list_mode(ui, ctx, busy, &mut request),
            LibraryPanelMode::Entry(entry_id) => {
                self.draw_entry_mode(ui, ctx, busy, &entry_id, &mut request);
            }
        }
        request
    }

    /// Draws the panel-wide status message, where the user is looking.
    ///
    /// Both screens call this from INSIDE their own scroll area, next to the control the user
    /// just used, and never from the top of the panel body. A grey `ui.small` pinned above the
    /// scroll area was the previous home of every capture failure, and it is exactly the place
    /// a user who has just dragged a rectangle over the canvas is not reading: the message was
    /// on screen and still unseen. It is a bordered, coloured block with a dismiss button, so
    /// it reads as an answer rather than as a caption.
    fn draw_status(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else {
            return;
        };
        let mut dismiss = false;
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.colored_label(ms_theme::status::WARNING, status);
                dismiss = ui
                    .button(t!("cleaning.tools.watermark.chapter.library_status_dismiss_button"))
                    .clicked();
            });
        });
        if dismiss {
            self.status = None;
        }
    }

    /// Draws the action row shared by both screens.
    fn draw_header(&mut self, ui: &mut egui::Ui, busy: bool) {
        // The row WRAPS: this body used to be a 620-pt window and is now a panel the user may
        // narrow, so four buttons side by side have to break onto a second row rather than
        // disappear behind a horizontal scrollbar (the rule `draw_clean_tab_body` states in
        // `crates/ms-tab-cleaning/src/MODULE_README.md`).
        ui.horizontal_wrapped(|ui| {
            // Inside a wrapping layout egui defaults widget text to `TextWrapMode::Wrap`
            // (`egui-0.35.0/src/ui.rs:588-600`), which would break a caption over two lines
            // instead of moving its button to the next row.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_refresh_button"
                    )),
                )
                .clicked()
            {
                self.listed = false;
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.reference_add_button"
                    )),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.reference_add_hint"))
                .clicked()
            {
                self.start_picker(PickerPurpose::ReferenceCrops);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_import_zip_button"
                    )),
                )
                .clicked()
            {
                self.start_picker(PickerPurpose::ImportArchive);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_import_dir_button"
                    )),
                )
                .clicked()
            {
                self.start_picker(PickerPurpose::ImportFolder);
            }
            if busy {
                ui.spinner();
            }
        });
    }

    /// Draws the card list and the «+ новый» button under it.
    fn draw_list_mode(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &LibraryPanelContext<'_>,
        busy: bool,
        request: &mut Option<LibraryPanelRequest>,
    ) {
        if self.entries.is_empty() {
            ui.small(t!("cleaning.tools.watermark.chapter.library_empty_hint"));
        }
        let mut action: Option<EntryAction> = None;
        egui::ScrollArea::vertical()
            .id_salt("cleaning.tools.watermark.library_window_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for index in 0..self.entries.len() {
                    ui.push_id(index, |ui| {
                        if let Some(requested) = self.draw_entry(ui, index, busy, ctx, request) {
                            action = Some(requested);
                        }
                    });
                }
                self.draw_status(ui);
                // «+ новый» CREATES the entry and opens it. It used to arm the canvas instead,
                // which made a new mark demand a selection before it existed anywhere — the
                // engine's "one background cannot separate alpha from the mark" rule applied at
                // creation time, where it does not belong. It belongs to FITTING a model, and
                // an entry with no samples and one with a single sample are both states the
                // store already expresses.
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!(
                            "cleaning.tools.watermark.chapter.library_new_entry_button"
                        )),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.library_new_entry_hint"))
                    .clicked()
                {
                    action = Some(EntryAction::CreateEmpty);
                }
            });
        if let Some(action) = action {
            self.run_entry_action(action);
        }
    }

    /// Draws one stored entry's own screen: name, per-entry actions, samples, pending crops.
    fn draw_entry_mode(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &LibraryPanelContext<'_>,
        busy: bool,
        entry_id: &str,
        request: &mut Option<LibraryPanelRequest>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .cloned()
        else {
            // The listing has not arrived yet (or the entry is gone): show nothing but the way
            // back, rather than an empty screen with no exit.
            self.draw_back_button(ui, request);
            return;
        };
        self.draw_back_button(ui, request);
        let mut name = self
            .edits
            .get(&entry.id)
            .cloned()
            .unwrap_or_else(|| entry.name.clone());
        let mut action: Option<EntryAction> = None;
        let armed = self.confirm_delete.as_deref() == Some(entry.id.as_str());
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.watermark.chapter.reference_name_label"));
            ui.add_enabled_ui(!busy, |ui| {
                // The name is user data: whatever is typed is kept verbatim.
                ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .id_salt("cleaning.tools.watermark.library_name")
                        .desired_width(NAME_EDIT_WIDTH),
                );
            });
            if ui
                .add_enabled(
                    !busy && name != entry.name,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_rename_button"
                    )),
                )
                .clicked()
            {
                action = Some(EntryAction::Rename(entry.id.clone(), name.clone()));
            }
        });
        self.edits.insert(entry.id.clone(), name);
        // Wraps for the same reason as the header row.
        ui.horizontal_wrapped(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.reference_improve_button"
                    )),
                )
                .on_hover_text(t!(
                    "cleaning.tools.watermark.chapter.reference_improve_hint"
                ))
                .clicked()
            {
                action = Some(EntryAction::Improve(entry.id.clone()));
            }
            // Offered only for an entry that HAS a mark: there is no footprint to measure on an
            // empty one, and a disabled button is the honest way to say so.
            if ui
                .add_enabled(
                    !busy && entry.has_template,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_trim_button"
                    )),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.library_trim_hint"))
                .clicked()
            {
                action = Some(EntryAction::TrimFootprint(entry.id.clone()));
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_export_zip_button"
                    )),
                )
                .clicked()
            {
                action = Some(EntryAction::ExportArchive(entry.id.clone()));
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_export_dir_button"
                    )),
                )
                .clicked()
            {
                action = Some(EntryAction::ExportFolder(entry.id.clone()));
            }
            let label = if armed {
                t!("cleaning.tools.watermark.chapter.library_delete_confirm_button")
            } else {
                t!("cleaning.tools.watermark.chapter.library_delete_button")
            };
            if ui
                .add_enabled(!busy, egui::Button::new(label))
                .on_hover_text(t!("cleaning.tools.watermark.chapter.library_delete_hint"))
                .clicked()
            {
                action = Some(if armed {
                    EntryAction::Delete(entry.id.clone())
                } else {
                    // Arming rather than deleting on the first click: the entry may be the
                    // only measurement of a mark that took a chapter to collect.
                    EntryAction::ArmDelete(entry.id.clone())
                });
            }
        });
        draw_entry_report(ui, &entry);
        ui.separator();
        let mut commit = false;
        // The armed sample belongs to THIS entry's row list; an arm left over from another
        // entry must not light up a row here.
        let armed_sample = self
            .confirm_delete_sample
            .as_ref()
            .filter(|(id, _)| id == &entry.id)
            .map(|(_, index)| *index);
        let mut sample_action: Option<EntryAction> = None;
        egui::ScrollArea::vertical()
            .id_salt("cleaning.tools.watermark.library_window_samples")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.label(t!("cleaning.tools.watermark.chapter.library_samples_heading"));
                sample_action = Self::draw_sample_rows(
                    ui,
                    self.samples.as_ref(),
                    &entry.id,
                    entry.updated_unix,
                    busy,
                    armed_sample,
                );
                // Refusals first, crops second, then the status and the arm control: the whole
                // answer to "what happened to my last drag" sits together, immediately above
                // the button that made it.
                self.draw_capture_refusals(ui, &entry.id);
                commit = self.draw_pending_rows(ui, busy, &entry.id);
                self.draw_status(ui);
                draw_arm_control(ui, ctx, busy, &entry.id, request);
            });
        if commit {
            self.commit_pending(&entry.id, ctx.sample_params, ctx.max_side);
        }
        // The header buttons win over a sample row when both fired in one frame: only one job
        // can start anyway, and the entry-wide action is the more destructive of the two.
        if let Some(action) = action.or(sample_action) {
            self.run_entry_action(action);
        }
    }

    /// Draws the «← К списку» affordance.
    ///
    /// The user's specification does not ask for one; without it the edit screen is a trap,
    /// because the card list is the only place the panel's other entries exist. Leaving the
    /// screen also drops any reservation: the arming was made for THIS screen.
    fn draw_back_button(&mut self, ui: &mut egui::Ui, request: &mut Option<LibraryPanelRequest>) {
        if ui
            .button(t!("cleaning.tools.watermark.chapter.library_back_button"))
            .clicked()
        {
            *request = Some(self.leave_entry_mode());
        }
    }

    /// Returns the panel to the card list and gives up any reservation it made.
    ///
    /// The reservation goes with the screen: «+ Выделить новый» was armed FOR one entry, and
    /// leaving that entry must not leave the next canvas selection pointing at it.
    fn leave_entry_mode(&mut self) -> LibraryPanelRequest {
        self.mode = LibraryPanelMode::List;
        // The armed sample delete goes with the screen too: its index addresses a row list the
        // user is walking away from.
        self.confirm_delete_sample = None;
        LibraryPanelRequest::Disarm
    }

    /// Draws the stored calibration samples of one entry.
    ///
    /// A MEASURED background is shown read-only: overwriting a measurement with a claim is
    /// exactly what this screen must never offer. A background the user asserted is shown as
    /// such, with the level they stated, because it is why the entry stopped claiming to be
    /// measured.
    ///
    /// `armed` is the row index whose delete button is waiting for its second click; the
    /// returned action is the one the user asked for, applied by the caller after the draw
    /// because it mutates the state this list is drawn from.
    fn draw_sample_rows(
        ui: &mut egui::Ui,
        samples: Option<&EntrySamples>,
        entry_id: &str,
        revision: u64,
        busy: bool,
        armed: Option<usize>,
    ) -> Option<EntryAction> {
        // Both halves of the key are checked: a list read before the entry was rewritten
        // describes samples that no longer exist, and showing it would be a lie about what the
        // entry now contains.
        let Some(samples) = samples
            .filter(|samples| samples.entry_id == entry_id && samples.revision == revision)
        else {
            ui.small(t!(
                "cleaning.tools.watermark.chapter.library_samples_loading_hint"
            ));
            return None;
        };
        if samples.rows.is_empty() {
            ui.small(t!(
                "cleaning.tools.watermark.chapter.library_samples_empty_hint"
            ));
            return None;
        }
        // The LAST remaining crop is the one whose deletion destroys the model, so its button
        // warns differently. The count is read from the rows actually on screen, which is the
        // same list the delete index addresses.
        let last_one = samples.rows.len() == 1;
        let mut action: Option<EntryAction> = None;
        for (index, row) in samples.rows.iter().enumerate() {
            ui.push_id(("cleaning.tools.watermark.library_sample", index), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        match row.texture.as_ref() {
                            Some(texture) => {
                                ui.add(egui::Image::new((
                                    texture.id(),
                                    egui::vec2(SAMPLE_PREVIEW_SIDE, SAMPLE_PREVIEW_SIDE),
                                )));
                            }
                            None => {
                                ui.allocate_space(egui::vec2(
                                    SAMPLE_PREVIEW_SIDE,
                                    SAMPLE_PREVIEW_SIDE,
                                ));
                            }
                        }
                        ui.vertical(|ui| {
                            ui.small(&row.file);
                            match row.background {
                                // A ring the page edge truncated gets its own, amber line.
                                // The level is still a measurement, so this is NOT the
                                // hand-asserted wording — but it rests on fewer, one-sided
                                // background pixels, and a row that said only "measured"
                                // would let that read as a full ring.
                                StoredSampleBackground::Flat {
                                    level,
                                    ring_std,
                                    ring_pixels: Some(pixels),
                                    ring_full_pixels: Some(full),
                                } if pixels < full => {
                                    ui.colored_label(
                                        ms_theme::status::WARNING,
                                        tf!(
                                            "cleaning.tools.watermark.chapter.library_sample_partial_line",
                                            level = format_level(level),
                                            std = format!("{:.1}", channel_max(ring_std)),
                                            pixels = pixels,
                                            full = full
                                        ),
                                    );
                                }
                                StoredSampleBackground::Flat { level, ring_std, .. } => {
                                    ui.small(tf!(
                                        "cleaning.tools.watermark.chapter.library_sample_measured_line",
                                        level = format_level(level),
                                        std = format!("{:.1}", channel_max(ring_std))
                                    ));
                                }
                                StoredSampleBackground::Manual { level } => {
                                    ui.colored_label(
                                        ms_theme::status::WARNING,
                                        tf!(
                                            "cleaning.tools.watermark.chapter.library_sample_manual_line",
                                            level = format_level(level)
                                        ),
                                    );
                                }
                            }
                            ui.small(match row.origin {
                                StoredSampleOrigin::Page { page_index, x, y } => tf!(
                                    "cleaning.tools.watermark.chapter.library_sample_page_origin",
                                    page = page_index + 1,
                                    x = x,
                                    y = y
                                ),
                                StoredSampleOrigin::ReferenceCrop => t!(
                                    "cleaning.tools.watermark.chapter.library_sample_reference_origin"
                                )
                                .to_string(),
                            });
                            // Deleting a crop REFITS the entry from the ones that remain — the
                            // stored `c`/`s` were fitted from all of them — so the button is
                            // armed on the first click and acts on the second, exactly like the
                            // entry's own delete.
                            let armed_here = armed == Some(index);
                            let confirm_label = t!(
                                "cleaning.tools.watermark.chapter.library_sample_delete_confirm_button"
                            )
                            .to_string();
                            let label = if armed_here {
                                confirm_label.clone()
                            } else {
                                t!("cleaning.tools.watermark.chapter.library_sample_delete_button")
                                    .to_string()
                            };
                            let hint = if last_one {
                                tf!(
                                    "cleaning.tools.watermark.chapter.library_sample_delete_last_hint",
                                    button = confirm_label
                                )
                            } else {
                                tf!(
                                    "cleaning.tools.watermark.chapter.library_sample_delete_hint",
                                    button = confirm_label
                                )
                            };
                            if ui
                                .add_enabled(!busy, egui::Button::new(label))
                                .on_hover_text(hint)
                                .clicked()
                            {
                                action = Some(if armed_here {
                                    EntryAction::DeleteSample(entry_id.to_string(), index)
                                } else {
                                    EntryAction::ArmDeleteSample(entry_id.to_string(), index)
                                });
                            }
                        });
                    });
                });
            });
        }
        action
    }

    /// Draws the capture-lane refusals aimed at `target`, each with its own dismiss button.
    ///
    /// Visually identical to the refusal block a refused INTAKE puts on a pending row, because
    /// the user is answering the same question with it — "why is my sample not in the list?" —
    /// and the lane that produced it is an implementation detail they should never have to
    /// learn. A refusal row owns no crop and no scratch file, so dismissing it frees nothing
    /// and loses nothing.
    fn draw_capture_refusals(&mut self, ui: &mut egui::Ui, target: &str) {
        if !self.capture_refusals.iter().any(|row| row.target == target) {
            return;
        }
        let mut dismiss: Option<usize> = None;
        for index in 0..self.capture_refusals.len() {
            if self.capture_refusals[index].target != target {
                continue;
            }
            ui.push_id(("cleaning.tools.watermark.library_capture_refusal", index), |ui| {
                ui.group(|ui| {
                    let row = &self.capture_refusals[index];
                    ui.colored_label(
                        ms_theme::status::ERROR,
                        t!("cleaning.tools.watermark.chapter.library_refused_heading"),
                    );
                    ui.small(row.reason.as_str());
                    if let Some(advice) = row.advice.as_ref() {
                        ui.small(advice.as_str());
                    }
                    if ui
                        .button(t!(
                            "cleaning.tools.watermark.chapter.library_pending_discard_button"
                        ))
                        .clicked()
                    {
                        dismiss = Some(index);
                    }
                });
            });
        }
        if let Some(index) = dismiss {
            self.capture_refusals.remove(index);
        }
    }

    /// Draws the crops captured for `target` that are not in the library yet.
    ///
    /// Returns `true` when the user asked to commit them. A crop keeps its colour control
    /// because a level the engine REFUSED to measure is the one thing only the user can
    /// supply; the eyedropper samples the page itself, which is where the answer is.
    fn draw_pending_rows(&mut self, ui: &mut egui::Ui, busy: bool, target: &str) -> bool {
        let count = self
            .pending
            .iter()
            .filter(|crop| crop.target == target)
            .count();
        if count == 0 {
            return false;
        }
        ui.label(t!("cleaning.tools.watermark.chapter.library_pending_heading"));
        // The "state the colour" hint belongs to crops that still need one. A crop the engine
        // MEASURED and the intake then refused needs a different drag, not a colour, and
        // telling it to pick a colour would send the user the wrong way.
        if self
            .pending
            .iter()
            .any(|crop| crop.target == target && !crop.stated)
        {
            ui.small(t!(
                "cleaning.tools.watermark.chapter.library_pending_needs_color_hint"
            ));
        }
        let mut discard: Option<usize> = None;
        for index in 0..self.pending.len() {
            if self.pending[index].target != target {
                continue;
            }
            ui.push_id(("cleaning.tools.watermark.library_pending", index), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let crop = &mut self.pending[index];
                        match crop.texture.as_ref() {
                            Some(texture) => {
                                ui.add(egui::Image::new((
                                    texture.id(),
                                    egui::vec2(SAMPLE_PREVIEW_SIDE, SAMPLE_PREVIEW_SIDE),
                                )));
                            }
                            None => {
                                ui.allocate_space(egui::vec2(
                                    SAMPLE_PREVIEW_SIDE,
                                    SAMPLE_PREVIEW_SIDE,
                                ));
                            }
                        }
                        ui.vertical(|ui| {
                            // The refusal comes FIRST: it is the answer to the question the
                            // user is asking when they look at this row ("why is my sample not
                            // in the list?"), and the colour control below it is only
                            // sometimes part of the fix.
                            if let Some(refusal) = self.pending[index].refusal.as_ref() {
                                ui.colored_label(
                                    ms_theme::status::ERROR,
                                    t!("cleaning.tools.watermark.chapter.library_refused_heading"),
                                );
                                ui.small(refusal.reason.as_str());
                                if let Some(advice) = refusal.advice.as_ref() {
                                    ui.small(advice.as_str());
                                }
                            }
                            let crop = &mut self.pending[index];
                            ui.small(t!(
                                "cleaning.tools.watermark.chapter.library_pending_color_label"
                            ));
                            // The response is what turns the control's starting guess into a
                            // stated level: until the user actually picks a colour, the crop
                            // has no background and must not be committed.
                            if crop.picker.draw(ui, &mut crop.level).changed {
                                crop.stated = true;
                            }
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!(
                                        "cleaning.tools.watermark.chapter.library_pending_discard_button"
                                    )),
                                )
                                .clicked()
                            {
                                discard = Some(index);
                            }
                        });
                    });
                });
            });
        }
        // Every crop here belongs to an entry that already exists, so one is enough: a single
        // known background level fits a deposit-exact model whose deposit is measured. What is
        // NOT optional is the level — a crop whose colour the user never stated would record a
        // claim nobody made.
        let enough = self.pending_ready(target);
        let commit = ui
            .add_enabled(
                !busy && enough,
                egui::Button::new(t!(
                    "cleaning.tools.watermark.chapter.library_pending_commit_button"
                )),
            )
            .on_disabled_hover_text(t!(
                "cleaning.tools.watermark.chapter.library_pending_needs_color_hint"
            ))
            .clicked();
        if let Some(index) = discard {
            let crop = self.pending.remove(index);
            remove_scratch_crop(&crop.path);
        }
        commit
    }

    /// The verbatim display name of one listed entry, or its id when it is gone.
    fn entry_name(&self, entry_id: &str) -> String {
        self.entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .map_or_else(|| entry_id.to_string(), |entry| entry.name.clone())
    }

    /// Draws the reference-crop form: the picked files, the new entry's name and the
    /// button that starts the intake.
    fn draw_intake(&mut self, ui: &mut egui::Ui, sample_params: SampleParams, max_side: u32) {
        if !self.intake.open {
            return;
        }
        // Everything the form draws is copied out of `self` first, so no closure below holds
        // a borrow of the window while another one needs it.
        let busy = self.busy();
        let improving = self.intake.target.clone();
        let heading = match improving.as_ref() {
            Some(entry_id) => tf!(
                "cleaning.tools.watermark.chapter.reference_improve_heading",
                name = self.entry_name(entry_id)
            ),
            None => t!("cleaning.tools.watermark.chapter.reference_create_heading").to_string(),
        };
        let files: Vec<String> = self
            .intake
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        let enough = self.intake.files.len() >= DRAFT_MIN_CROPS || improving.is_some();
        let mut name = self.intake.name.clone();
        let mut run = false;
        let mut pick = false;
        let mut cancel = false;
        ui.group(|ui| {
            ui.label(heading);
            for file in &files {
                ui.small(file);
            }
            if improving.is_none() {
                ui.horizontal(|ui| {
                    ui.label(t!("cleaning.tools.watermark.chapter.reference_name_label"));
                    // The name is user data: whatever is typed is kept verbatim.
                    ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .id_salt("cleaning.tools.watermark.library_intake_name")
                            .desired_width(NAME_EDIT_WIDTH),
                    );
                });
            }
            // Wraps for the same reason as the top action row.
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                let run_label = if improving.is_some() {
                    t!("cleaning.tools.watermark.chapter.reference_improve_run_button")
                } else {
                    t!("cleaning.tools.watermark.chapter.reference_run_button")
                };
                run = ui
                    .add_enabled(!busy && enough, egui::Button::new(run_label))
                    .on_disabled_hover_text(t!(
                        "cleaning.tools.watermark.chapter.reference_needs_two_error"
                    ))
                    .clicked();
                pick = ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!(
                            "cleaning.tools.watermark.chapter.reference_pick_button"
                        )),
                    )
                    .clicked();
                cancel = ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!("cleaning.common.cancel_button")),
                    )
                    .clicked();
            });
        });
        self.intake.name = name;
        if cancel {
            self.intake.reset();
            return;
        }
        if pick {
            let purpose = improving.map_or(PickerPurpose::ReferenceCrops, |entry_id| {
                PickerPurpose::ImproveEntry(entry_id)
            });
            self.start_picker(purpose);
            return;
        }
        if run {
            self.start_intake(sample_params, max_side);
        }
    }

    /// Draws one entry's card: the mark on white, its name, its warnings and its two
    /// controls. Returns the action the user asked for.
    ///
    /// Per-entry actions do NOT live here — they moved to the edit screen with the name
    /// editor, so the card stays what the list is for: deciding which mark to hunt.
    fn draw_entry(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        busy: bool,
        ctx: &LibraryPanelContext<'_>,
        request: &mut Option<LibraryPanelRequest>,
    ) -> Option<EntryAction> {
        // Same discipline as the intake form: the card draws from locals only, so the
        // closures never hold a borrow of the window.
        let entry = self.entries.get(index)?.clone();
        // Cloning a `TextureHandle` is a refcount bump, not a copy of the image.
        let icon = self
            .icons
            .get(&entry.id)
            .filter(|icon| icon.revision == entry.updated_unix)
            .map(|icon| icon.texture.clone());
        let membership = entry_membership(&entry.id, ctx);
        let mut action = None;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                match icon.as_ref() {
                    Some(texture) => {
                        ui.add(egui::Image::new((
                            texture.id(),
                            egui::vec2(PREVIEW_SIDE, PREVIEW_SIDE),
                        )));
                    }
                    // No icon. Two different reasons, and the card has to tell them apart: an
                    // EMPTY entry has no mark to draw and never will until it is given one, so
                    // it says so in the slot; anything else is a render still on its worker and
                    // holds the card's shape so it does not jump when the texture arrives.
                    None => {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(PREVIEW_SIDE, PREVIEW_SIDE),
                            egui::Sense::hover(),
                        );
                        if !entry.has_template {
                            ui.put(
                                rect,
                                egui::Label::new(
                                    egui::RichText::new(t!(
                                        "cleaning.tools.watermark.chapter.library_entry_empty_icon"
                                    ))
                                    .small()
                                    .color(ms_theme::status::WARNING),
                                )
                                .wrap(),
                            );
                        }
                    }
                }
                ui.vertical(|ui| {
                    // The name is USER DATA and is shown verbatim; editing it lives on the
                    // entry's own screen.
                    ui.label(&entry.name);
                    // Wraps for the same reason as the header row.
                    ui.horizontal_wrapped(|ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        let (label, hint) = match membership {
                            EntryMembership::Absent => (
                                t!("cleaning.tools.watermark.chapter.library_select_button"),
                                t!("cleaning.tools.watermark.chapter.library_select_hint"),
                            ),
                            EntryMembership::Loaded => (
                                t!("cleaning.tools.watermark.chapter.library_unselect_button"),
                                t!("cleaning.tools.watermark.chapter.library_unselect_hint"),
                            ),
                            EntryMembership::SavedFromChapter => (
                                t!("cleaning.tools.watermark.chapter.library_in_chapter_button"),
                                t!("cleaning.tools.watermark.chapter.library_in_chapter_hint"),
                            ),
                        };
                        // A mark saved FROM the chapter is already being hunted and is not the
                        // card's to load or to drop, so its control is shown pressed and
                        // disabled: the displayed state and the click have to agree, or the
                        // click pushes a second kind for one physical mark.
                        // An EMPTY entry cannot be loaded into the chapter: it has no template
                        // to correlate against and no model to remove with. The card refuses it
                        // here rather than letting the worker discover it, so the displayed
                        // state and the click still agree.
                        let clickable = !ctx.chapter_busy
                            && membership != EntryMembership::SavedFromChapter
                            && entry.has_template;
                        let selected = membership != EntryMembership::Absent;
                        if ui
                            .add_enabled(clickable, egui::Button::new(label).selected(selected))
                            .on_hover_text(hint)
                            .on_disabled_hover_text(hint)
                            .clicked()
                        {
                            match membership {
                                EntryMembership::Loaded => {
                                    *request = Some(LibraryPanelRequest::UnselectEntry(
                                        entry.id.clone(),
                                    ));
                                }
                                EntryMembership::Absent => {
                                    *request =
                                        Some(LibraryPanelRequest::SelectEntry(entry.id.clone()));
                                }
                                // Not reachable: the control is disabled in this state. Asking
                                // for anything here is exactly the duplicate this state exists
                                // to prevent.
                                EntryMembership::SavedFromChapter => {}
                            }
                        }
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_edit_button"
                                )),
                            )
                            .clicked()
                        {
                            action = Some(EntryAction::Edit(entry.id.clone()));
                        }
                    });
                    draw_entry_report(ui, &entry);
                });
            });
        });
        action
    }

    /// Starts the worker one card or one edit-screen button asked for.
    fn run_entry_action(&mut self, action: EntryAction) {
        match action {
            EntryAction::CreateEmpty => {
                // The name is a placeholder the user renames on the screen this opens: the
                // entry has to exist before anything can be typed into it, and the rename
                // control is already there. It is stored verbatim like any other name.
                let name = t!("cleaning.tools.watermark.chapter.library_new_entry_name").to_string();
                self.start_job(move |tx| {
                    let request = empty_entry_request(name);
                    let _ = tx.send(match save_entry(&request) {
                        Ok(entry_id) => LibraryEvent::Created {
                            status: tf!(
                                "cleaning.tools.watermark.chapter.library_created_status",
                                id = entry_id.clone()
                            ),
                            entry_id,
                        },
                        Err(err) => {
                            ms_log::runtime_log::log_warn(format!(
                                "[cleaning] watermark library: an empty entry could not be \
                                 created: {err}"
                            ));
                            LibraryEvent::failed(err)
                        }
                    });
                });
            }
            EntryAction::Edit(entry_id) => {
                // A row index means nothing on another entry's screen, so the arm does not
                // travel with the user.
                self.confirm_delete_sample = None;
                self.mode = LibraryPanelMode::Entry(entry_id);
            }
            EntryAction::ArmDelete(entry_id) => {
                self.confirm_delete = Some(entry_id);
            }
            EntryAction::Rename(entry_id, name) => {
                self.start_job(move |tx| {
                    let _ = tx.send(match rename_entry(&entry_id, &name) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_renamed_status",
                            name = name
                        )),
                        Err(err) => LibraryEvent::failed(err),
                    });
                });
            }
            EntryAction::Delete(entry_id) => {
                self.confirm_delete = None;
                // The entry is about to stop existing, so its own screen must not stay up.
                self.mode = LibraryPanelMode::List;
                self.start_job(move |tx| {
                    let _ = tx.send(match delete_entry(&entry_id) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_deleted_status",
                            id = entry_id
                        )),
                        Err(err) => LibraryEvent::failed(err),
                    });
                });
            }
            EntryAction::ArmDeleteSample(entry_id, index) => {
                self.confirm_delete_sample = Some((entry_id, index));
            }
            EntryAction::DeleteSample(entry_id, index) => {
                self.confirm_delete_sample = None;
                // Everything here is worker work: a decode per remaining crop, a rayon fit and
                // a full directory rewrite. None of it may touch the GUI thread.
                self.start_job(move |tx| {
                    let written = load_entry(&entry_id)
                        .and_then(|entry| drop_entry_sample(entry, index))
                        .and_then(|request| {
                            let remaining = request.samples.len();
                            save_entry(&request).map(|id| (id, remaining))
                        });
                    let _ = tx.send(match written {
                        Ok((id, remaining)) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_sample_deleted_status",
                            id = id,
                            count = remaining
                        )),
                        Err(err) => {
                            ms_log::runtime_log::log_warn(format!(
                                "[cleaning] watermark library: sample {index} of entry \
                                 {entry_id} could not be deleted: {err}"
                            ));
                            LibraryEvent::failed(err)
                        }
                    });
                });
            }
            EntryAction::TrimFootprint(entry_id) => {
                // A decode per stored crop, two rayon fits and a full directory rewrite: worker
                // work from end to end, exactly like dropping a sample.
                self.start_job(move |tx| {
                    let outcome = load_entry(&entry_id).and_then(|entry| {
                        trim_entry_footprint(entry).and_then(|outcome| match outcome {
                            FootprintTrimOutcome::NoMark => Ok(t!(
                                "cleaning.tools.watermark.chapter.library_trim_no_mark_status"
                            )
                            .to_string()),
                            FootprintTrimOutcome::AlreadyTight { footprint } => Ok(tf!(
                                "cleaning.tools.watermark.chapter.library_trim_already_tight_status",
                                width = footprint.0,
                                height = footprint.1
                            )),
                            FootprintTrimOutcome::Trimmed {
                                request,
                                before,
                                after,
                                offset,
                            } => save_entry(&request).map(|_| {
                                ms_log::runtime_log::log_info(format!(
                                    "[cleaning] watermark library: entry {entry_id} footprint \
                                     trimmed from {}x{} to {}x{} at offset ({}, {}); anchors \
                                     shifted by {}",
                                    before.0, before.1, after.0, after.1, offset.0, offset.1,
                                    offset.0
                                ));
                                tf!(
                                    "cleaning.tools.watermark.chapter.library_trimmed_status",
                                    before_width = before.0,
                                    before_height = before.1,
                                    width = after.0,
                                    height = after.1
                                )
                            }),
                        })
                    });
                    let _ = tx.send(match outcome {
                        Ok(status) => LibraryEvent::Changed(status),
                        Err(err) => {
                            ms_log::runtime_log::log_warn(format!(
                                "[cleaning] watermark library: the footprint of entry \
                                 {entry_id} could not be trimmed: {err}"
                            ));
                            LibraryEvent::failed(err)
                        }
                    });
                });
            }
            EntryAction::Improve(entry_id) => {
                self.start_picker(PickerPurpose::ImproveEntry(entry_id));
            }
            EntryAction::ExportArchive(entry_id) => {
                self.start_picker(PickerPurpose::ExportArchive(entry_id));
            }
            EntryAction::ExportFolder(entry_id) => {
                self.start_picker(PickerPurpose::ExportFolder(entry_id));
            }
        }
    }

    /// Commits every pending crop of `target` through the reference intake.
    ///
    /// The asserted level is consulted by the intake ONLY for a crop whose ring it refuses to
    /// call flat; a crop that measures flat keeps its measurement, so stating a level can
    /// never weaken the automatic test.
    ///
    /// The rows are NOT removed here. They leave when the worker confirms the entry was
    /// written (`drop_committed_pending`): a refused intake keeps its scratch files precisely
    /// so the user can correct a level and commit again, and rows taken away before the answer
    /// would make those files unreachable from every screen.
    fn commit_pending(&mut self, target: &str, sample_params: SampleParams, max_side: u32) {
        // Re-checked here and not only on the button: the gate is a correctness rule about
        // what may be written, not a piece of button styling.
        if !self.pending_ready(target) {
            return;
        }
        let taken: Vec<&PendingCrop> = self
            .pending
            .iter()
            .filter(|crop| crop.target == target)
            .collect();
        let files: Vec<PathBuf> = taken.iter().map(|crop| crop.path.clone()).collect();
        let levels: Vec<Option<[f32; 3]>> = taken
            .iter()
            .map(|crop| Some(color_to_level(crop.level)))
            .collect();
        let margins: Vec<Option<CropMargins>> =
            taken.iter().map(|crop| Some(crop.margins)).collect();
        self.run_intake(IntakeJob {
            manual_levels: levels,
            crop_margins: margins,
            // Never creates an entry: these crops always improve one that already exists.
            name: String::new(),
            target: Some(target.to_string()),
            sample_params,
            max_side,
            scratch: files.clone(),
            files,
        });
    }
}

/// What one card or one edit-screen button asked the window to do. Collected during the draw
/// and executed after it, because every one of these mutates the state it is drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryAction {
    /// Create an EMPTY entry and open its screen. What «+ новый» asks for.
    CreateEmpty,
    /// Open this entry's own screen.
    Edit(String),
    /// First click of the two-click delete.
    ArmDelete(String),
    Rename(String, String),
    Delete(String),
    /// First click of the two-click delete of one stored calibration crop: entry id and the
    /// crop's index in the row list currently on screen.
    ArmDeleteSample(String, usize),
    /// Second click: drop that crop, refit the entry from the rest and rewrite it.
    DeleteSample(String, usize),
    /// Re-derive this entry's footprint from the mark it actually holds and rewrite it.
    TrimFootprint(String),
    Improve(String),
    ExportArchive(String),
    ExportFolder(String),
}

/// Draws the arming control: «+ новый» / «+ Выделить новый», or the red cancel that replaces
/// it while a reservation is in force.
///
/// The control is drawn from the TOOL's reservation, never from a copy kept here: exactly one
/// reservation exists at a time, and a second copy would be a second answer to the question
/// "is the next selection spoken for".
fn draw_arm_control(
    ui: &mut egui::Ui,
    ctx: &LibraryPanelContext<'_>,
    busy: bool,
    arm: &str,
    request: &mut Option<LibraryPanelRequest>,
) {
    let cancel_label = t!("cleaning.tools.watermark.chapter.library_cancel_arm_button").to_string();
    if ctx.armed == Some(arm) {
        if ui
            .add(
                egui::Button::new(cancel_label.clone()).fill(Color32::from_rgb(
                    ARM_CANCEL_RGB[0],
                    ARM_CANCEL_RGB[1],
                    ARM_CANCEL_RGB[2],
                )),
            )
            .clicked()
        {
            *request = Some(LibraryPanelRequest::Disarm);
        }
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.library_arm_sample_hint",
            button = cancel_label
        ));
        // The GESTURE, on the surface the user is reading. The hint above says a selection is
        // expected but not how to make one, and the one place that does say
        // (`cleaning.region.selection_hint`) is painted in a DIFFERENT dock tab. It names the
        // gesture that is accepted while armed — a plain ЛКМ drag, Shift optional — which is
        // the contract `RegionEditToolBase::wants_primary_stroke` enforces in that state.
        ui.small(t!(
            "cleaning.tools.watermark.chapter.library_arm_gesture_hint"
        ));
        return;
    }
    let label = t!("cleaning.tools.watermark.chapter.library_add_sample_button");
    // Another reservation being in force is exactly why this button must still be clickable:
    // clicking it MOVES the reservation here, which is how "only one is live" stays true.
    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
        *request = Some(LibraryPanelRequest::Arm(arm.to_string()));
    }
}

/// What to DO about an intake refusal, for the refusals a different canvas drag can fix.
///
/// The engine's own message says what went wrong in its own terms — a size, a correlation
/// score, a measured level — which is precise and, on its own, not actionable: the user is
/// holding a mouse, not a ruler. These three sentences say which drag would have worked.
/// [`ReferenceRefusal::Other`] gets none, because there is no gesture that fixes a file that
/// will not decode.
fn refusal_advice(refusal: ReferenceRefusal) -> Option<String> {
    match refusal {
        ReferenceRefusal::TooSmall => Some(format!(
            "{} {}",
            t!("cleaning.tools.watermark.chapter.library_refused_too_small_advice"),
            // The other half of this refusal is often the ENTRY, not the drag: a footprint that
            // came from the chapter detector demands a crop several times the size of the mark
            // inside it, and no drag can be large enough at a page edge.
            t!("cleaning.tools.watermark.chapter.library_refused_too_small_trim_advice")
        )),
        ReferenceRefusal::Misaligned => {
            Some(t!("cleaning.tools.watermark.chapter.library_refused_align_advice").to_string())
        }
        ReferenceRefusal::Background => Some(
            t!("cleaning.tools.watermark.chapter.library_refused_background_advice").to_string(),
        ),
        ReferenceRefusal::Other => None,
    }
}

/// True when a colour control owns this frame's primary click, given the two flags one
/// [`ViewportColorSelector`] reports.
///
/// BOTH are needed, and that is the whole content of this function. `active` covers every
/// frame the eyedropper is sampling; `consumed` covers the ONE frame that ends the sampling,
/// on which the widget has already cleared `active`. The panel is drawn inside
/// `CanvasView::draw`, before the tool's input hook of the same frame, so that frame reaches
/// the canvas with `active == false` — and a guard written on `active` alone lets the click
/// that finished the colour pick start a selection drag under the sampled pixel.
fn eyedropper_owns_click(active: bool, consumed: bool) -> bool {
    active || consumed
}

/// What one entry is to the open chapter, and therefore what its card may offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryMembership {
    /// The chapter does not carry this mark: the card may load it.
    Absent,
    /// A mark was LOADED from this entry: the card may take that mark back out.
    Loaded,
    /// A mark the chapter discovered was SAVED to this entry. It is already being hunted
    /// under its own kind id, so loading it again would give the catalog two kinds for one
    /// physical mark — and unloading it would discard measurements a scan paid for.
    SavedFromChapter,
}

/// Classifies one entry against the chapter the tool reported this frame.
///
/// Pure and separate from the drawing because the rule is a correctness rule: the state the
/// card DISPLAYS and what its click does must come from one answer, or a card can show
/// «Выбрать» for a mark the chapter already has.
fn entry_membership(entry_id: &str, ctx: &LibraryPanelContext<'_>) -> EntryMembership {
    if ctx.selected_entries.iter().any(|id| id == entry_id) {
        return EntryMembership::Loaded;
    }
    if ctx.saved_entries.iter().any(|id| id == entry_id) {
        return EntryMembership::SavedFromChapter;
    }
    EntryMembership::Absent
}

/// One entry's verdict line: what it says and in which colour.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerdictLine {
    color: Color32,
    text: String,
}

/// Composes one entry's verdict line.
///
/// Pure, and separate from the drawing on purpose: the rule it enforces is a correctness
/// rule, not a layout one. An entry whose calibration rests on a HAND-STATED background is
/// never given the wording that claims a measurement — `verdict_separable` and
/// `verdict_deposit_exact` both say so literally — and that has to be testable without a
/// `Ui`.
///
/// The match over `ModelConditioning` is EXHAUSTIVE by design (AGENTS.md §17): every known
/// verdict gets an arm of its own, because reporting a known one through the unknown-verdict
/// fallback tells the user their build is out of date about an answer the build already has —
/// the chapter mode renders the same five verdicts correctly, and the two may not disagree.
/// The fallback is reached only when the stored tag maps to no verdict at all.
fn entry_verdict_line(warnings: &EntryWarnings, entry: &EntrySummary) -> VerdictLine {
    let asserted = warnings.rests_on_assertion;
    match warnings.conditioning.as_ref() {
        Some(ModelConditioning::Separable { .. }) if !asserted => VerdictLine {
            color: ms_theme::status::SUCCESS,
            text: t!("cleaning.tools.watermark.chapter.verdict_separable").to_string(),
        },
        Some(ModelConditioning::Separable { .. }) => VerdictLine {
            color: ms_theme::status::WARNING,
            text: t!("cleaning.tools.watermark.chapter.verdict_separable_asserted").to_string(),
        },
        Some(ModelConditioning::DepositExact { .. }) => VerdictLine {
            color: ms_theme::status::WARNING,
            text: if asserted {
                t!("cleaning.tools.watermark.chapter.verdict_deposit_exact_asserted").to_string()
            } else {
                t!("cleaning.tools.watermark.chapter.verdict_deposit_exact").to_string()
            },
        },
        Some(ModelConditioning::NotEnoughSamples { have, need }) => VerdictLine {
            color: ms_theme::status::WARNING,
            text: tf!(
                "cleaning.tools.watermark.chapter.verdict_not_enough",
                have = have,
                need = need
            ),
        },
        Some(ModelConditioning::DepositUnavailable { samples, spread }) => VerdictLine {
            color: ms_theme::status::WARNING,
            text: tf!(
                "cleaning.tools.watermark.chapter.verdict_deposit_unavailable",
                samples = samples,
                spread = format!("{spread:.0}")
            ),
        },
        Some(ModelConditioning::Underdetermined {
            underdetermined_pixels,
            total_pixels,
            worst_pixel_spread,
            required,
            ..
        }) => VerdictLine {
            color: ms_theme::status::WARNING,
            text: tf!(
                "cleaning.tools.watermark.chapter.verdict_underdetermined",
                pixels = underdetermined_pixels,
                total = total_pixels,
                spread = format!("{worst_pixel_spread:.0}"),
                required = format!("{required:.0}")
            ),
        },
        // A verdict this build does not know is reported by its literal tag rather than
        // squeezed into the closest known wording, which would misstate its quality. Only a
        // tag `conditioning_from_stored` could not map reaches this arm: every KNOWN verdict
        // has one of its own above, so no entry is ever told its build is out of date about a
        // verdict the build renders correctly two screens away.
        None => VerdictLine {
            color: ms_theme::status::WARNING,
            text: tf!(
                "cleaning.tools.watermark.chapter.library_verdict_unknown",
                verdict = entry.verdict.clone()
            ),
        },
    }
}

/// What one entry's icon is drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IconPlan {
    /// Composite the stored model onto white.
    Mark,
    /// The entry carries NO model, so the stored `template.png` stands in. Not a failure.
    Template,
    /// The entry carries neither a model nor a template: it is EMPTY, and there is no picture
    /// of a mark it has not been given yet. The card says so in words instead of showing one.
    Empty,
    /// Report this already-localized message; the card keeps its blank icon slot.
    Failed(String),
}

/// Decides what an entry's icon comes from, given the mark renderer's answer and whether the
/// entry has a template at all.
///
/// `Ok(None)` is the "no model" case and is NOT an error: the entry was written by a fit that
/// refused, there is nothing to composite, and the template is the only honest picture left —
/// unless there is no template either, which is an EMPTY entry and gets no picture at all
/// rather than an invented one. `Err` is a user-facing message and must reach the status line
/// rather than be swallowed into a blank card.
fn icon_plan(rendered: &Result<Option<image::RgbaImage>, String>, has_template: bool) -> IconPlan {
    match rendered {
        Ok(Some(_)) => IconPlan::Mark,
        Ok(None) if has_template => IconPlan::Template,
        Ok(None) => IconPlan::Empty,
        Err(err) => IconPlan::Failed(err.clone()),
    }
}

/// Draws one entry's quality verdict, its warnings, calibration levels and usage history.
///
/// This is the point of the whole screen: it must be readable at a glance which entries are
/// EXACT (two well-separated levels, the closed form) and which are graded — and for the
/// graded ones it names the background that would make them exact.
///
/// The one rule that is not presentation: an entry whose calibration rests on a HAND-STATED
/// background is never described as measured, whatever its verdict tag says. The verdict
/// grades the arithmetic; `rests_on_assertion` grades the evidence under it, and the evidence
/// is what the wording claims.
fn draw_entry_report(ui: &mut egui::Ui, entry: &EntrySummary) {
    let warnings = entry_warnings(entry);
    let asserted = warnings.rests_on_assertion;
    let verdict = entry_verdict_line(&warnings, entry);
    ui.colored_label(verdict.color, verdict.text);
    // An EMPTY entry says what it is and what to do with it, instead of leaving the user to
    // read "not enough samples" about a mark the entry does not even have yet.
    if warnings.is_empty {
        ui.colored_label(
            ms_theme::status::WARNING,
            t!("cleaning.tools.watermark.chapter.library_entry_empty_hint"),
        );
    }
    if warnings.partial_rings > 0 {
        ui.colored_label(
            ms_theme::status::WARNING,
            tf!(
                "cleaning.tools.watermark.chapter.library_partial_ring_warning",
                count = warnings.partial_rings
            ),
        );
    }
    if warnings.samples_disagree {
        ui.colored_label(
            ms_theme::status::WARNING,
            tf!(
                "cleaning.tools.watermark.chapter.library_samples_disagree_warning",
                percent = format!("{:.0}", warnings.clamped_share * 100.0)
            ),
        );
    }
    if asserted {
        ui.colored_label(
            ms_theme::status::WARNING,
            tf!(
                "cleaning.tools.watermark.chapter.library_manual_background_warning",
                files = warnings
                    .manual_backgrounds
                    .iter()
                    .map(|manual| manual.file.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    if let Some(alpha) = entry.alpha.as_ref() {
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.library_alpha_line",
            percent = format!("{:.0}", alpha.percent)
        ));
    }
    ui.small(if entry.levels.is_empty() {
        t!("cleaning.tools.watermark.chapter.levels_none").to_string()
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.levels_line",
            levels = entry
                .levels
                .iter()
                .map(|level| format!("{level:.0}"))
                .collect::<Vec<_>>()
                .join(", "),
            spread = format!("{:.0}", entry.spread)
        )
    });
    if let Some(suggestion) = warnings
        .conditioning
        .as_ref()
        .and_then(ModelConditioning::suggested_background)
    {
        ui.small(match suggestion {
            crate::watermark_chapter::SuggestedBackground::Darker { at_most } => {
                tf!(
                    "cleaning.tools.watermark.chapter.suggest_darker",
                    level = format!("{at_most:.0}")
                )
            }
            crate::watermark_chapter::SuggestedBackground::Brighter {
                at_least,
            } => tf!(
                "cleaning.tools.watermark.chapter.suggest_brighter",
                level = format!("{at_least:.0}")
            ),
        });
    }
    ui.small(tf!(
        "cleaning.tools.watermark.chapter.library_entry_line",
        name = entry.id.clone(),
        width = entry.width,
        height = entry.height,
        anchors = entry.anchor_key.clone(),
        samples = entry.samples
    ));
    ui.small(if entry.sources.is_empty() {
        t!("cleaning.tools.watermark.chapter.library_sources_none").to_string()
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.library_sources_line",
            sources = entry
                .sources
                .iter()
                .map(|source| format!(
                    "{} ({} px, {})",
                    source.source_key, source.page_width, source.anchor_key
                ))
                .collect::<Vec<_>>()
                .join("; ")
        )
    });
}

/// Renders one entry's card icon on a worker thread.
///
/// The icon is the MARK composited on white. An entry with no model has nothing to
/// composite, and only then does the stored `template.png` stand in — no icon is invented.
fn render_entry_icon(id: String, revision: u64, has_template: bool) -> IconRender {
    // An entry with no template has no planes either — `validate_entry_dir` refuses any other
    // combination — so reading them back would be an I/O round trip whose only possible answer
    // is `Ok(None)`. Skipping it is also what keeps the EMPTY case from being reported as a
    // failure on the one frame the listing is older than the disk.
    let rendered = if has_template {
        render_mark_on_white(&id)
    } else {
        Ok(None)
    };
    match icon_plan(&rendered, has_template) {
        IconPlan::Mark => IconRender {
            image: rendered.ok().flatten().as_ref().map(thumbnail),
            id,
            revision,
            error: None,
        },
        // Nothing to draw and nothing wrong: the card shows the entry's own "empty" wording in
        // the slot instead of a picture, which is the truth about an entry with no crops yet.
        IconPlan::Empty => IconRender {
            id,
            revision,
            image: None,
            error: None,
        },
        IconPlan::Template => match load_entry_template(&id) {
            // `Ok(None)` cannot reach here — `icon_plan` sent a template-less entry down the
            // `Empty` arm — but the listing it was decided from is a frame old, so an entry
            // whose template vanished in between reports nothing rather than inventing one.
            Ok(Some(template)) => IconRender {
                id,
                revision,
                image: Some(thumbnail(&template)),
                error: None,
            },
            Ok(None) => IconRender {
                id,
                revision,
                image: None,
                error: None,
            },
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] watermark library template icon for {id} failed: {err}"
                ));
                IconRender {
                    id,
                    revision,
                    image: None,
                    error: Some(err),
                }
            }
        },
        IconPlan::Failed(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark library mark icon for {id} failed: {err}"
            ));
            IconRender {
                id,
                revision,
                image: None,
                error: Some(err),
            }
        }
    }
}

/// Deletes one session scratch crop, reporting a failure to the log only.
///
/// A scratch file that survives is a few kilobytes in the system temp directory; failing the
/// user's operation over it would be worse than leaving it, so this never returns an error.
pub(super) fn remove_scratch_crop(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library scratch crop {} could not be removed: {err}",
            path.display()
        ));
    }
}

/// Downscales a decoded image into a thumbnail `egui::ColorImage`.
///
/// `pub(super)` so the tool's capture worker can build the preview of a canvas crop on its
/// own thread instead of handing the full-size crop to the GUI one. `ColorImage` is plain
/// data; only uploading it needs the `Context`.
pub(super) fn thumbnail(source: &image::RgbaImage) -> egui::ColorImage {
    let (width, height) = source.dimensions();
    let longest = width.max(height).max(1);
    let scaled = if longest > PREVIEW_MAX_PX {
        let target_w = (width * PREVIEW_MAX_PX / longest).max(1);
        let target_h = (height * PREVIEW_MAX_PX / longest).max(1);
        image::imageops::thumbnail(source, target_w, target_h)
    } else {
        source.clone()
    };
    egui::ColorImage::from_rgba_unmultiplied(
        [scaled.width() as usize, scaled.height() as usize],
        scaled.as_raw(),
    )
}

/// One background level as the card shows it: three rounded channels.
fn format_level(level: [f32; 3]) -> String {
    format!("{:.0}, {:.0}, {:.0}", level[0], level[1], level[2])
}

/// Largest of three channels, for a one-number summary of a per-channel measurement.
fn channel_max(values: [f32; 3]) -> f32 {
    values[0].max(values[1]).max(values[2])
}

/// A background level as a colour swatch.
fn level_to_color(level: [f32; 3]) -> Color32 {
    Color32::from_rgb(
        level_channel_to_u8(level[0]),
        level_channel_to_u8(level[1]),
        level_channel_to_u8(level[2]),
    )
}

/// One level channel as a byte.
///
/// Cast justification: the value is clamped into 0..=255 and rounded first, so it is exactly
/// representable as `u8`; a non-finite channel clamps to 0 rather than producing a garbage
/// byte.
fn level_channel_to_u8(value: f32) -> u8 {
    if value.is_finite() {
        value.clamp(0.0, 255.0).round() as u8
    } else {
        0
    }
}

/// A colour swatch as a background level, per channel 0..=255.
fn color_to_level(color: Color32) -> [f32; 3] {
    [
        f32::from(color.r()),
        f32::from(color.g()),
        f32::from(color.b()),
    ]
}

/// Spawns the blocking native file dialog for `purpose` on a worker thread.
///
/// Every variant answers with a path list so the caller has one shape to poll; single-pick
/// dialogs answer with one element. `None` means the user cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_picker(purpose: &PickerPurpose) -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    let purpose = purpose.clone();
    thread::spawn(move || {
        let picked = match purpose {
            PickerPurpose::ReferenceCrops | PickerPurpose::ImproveEntry(_) => {
                rfd::FileDialog::new()
                    .add_filter(
                        t!("cleaning.tools.watermark.chapter.reference_files_filter"),
                        &["png", "jpg", "jpeg", "webp", "bmp"],
                    )
                    .pick_files()
            }
            PickerPurpose::ImportArchive => rfd::FileDialog::new()
                .add_filter(
                    t!("cleaning.tools.watermark.chapter.library_archive_filter"),
                    &[ENTRY_ARCHIVE_EXTENSION],
                )
                .pick_file()
                .map(|path| vec![path]),
            PickerPurpose::ImportFolder | PickerPurpose::ExportFolder(_) => {
                rfd::FileDialog::new().pick_folder().map(|path| vec![path])
            }
            PickerPurpose::ExportArchive(entry_id) => rfd::FileDialog::new()
                .set_file_name(format!("{entry_id}.{ENTRY_ARCHIVE_EXTENSION}"))
                .add_filter(
                    t!("cleaning.tools.watermark.chapter.library_archive_filter"),
                    &[ENTRY_ARCHIVE_EXTENSION],
                )
                .save_file()
                .map(|path| vec![path]),
        };
        let _ = tx.send(picked);
    });
    rx
}

/// Web fallback: the browser build has no native file dialog (`rfd` is native-only), so the
/// pick resolves immediately as cancelled and the dropped capability is logged.
#[cfg(target_arch = "wasm32")]
fn spawn_picker(_purpose: &PickerPurpose) -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    ms_log::runtime_log::log_warn(
        "[cleaning] watermark library file picker unavailable on web build",
    );
    let _ = tx.send(None);
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::watermark_library::{ManualBackgroundRef, StoredAlpha};
    use super::super::watermark_removal::describe_conditioning;

    /// One listed entry with a graded verdict and no hand-stated background.
    fn summary(id: &str) -> EntrySummary {
        EntrySummary {
            has_template: true,
            partial_rings: 0,
            id: id.to_string(),
            name: format!("mark {id}"),
            width: 8,
            height: 8,
            anchor_key: "0".to_string(),
            verdict: "deposit_exact".to_string(),
            levels: vec![255.0],
            spread: 0.0,
            samples: 1,
            alpha: Some(StoredAlpha {
                source: "assumed".to_string(),
                percent: 30.0,
                rms_lsb: 1.0,
                dark_rms_lsb: 1.0,
                dark_max_lsb: 1.0,
                dark_luma: 32.0,
            }),
            fit_method: Some("closed_form_flat".to_string()),
            signature: None,
            sources: Vec::new(),
            updated_unix: 100,
            format: 1,
            clamped_pixels: 0,
            manual_backgrounds: Vec::new(),
        }
    }

    /// Pins the active catalog to English for the duration of a wording test.
    ///
    /// The catalog is a process-global `ArcSwap` and other tests in this crate switch it, so a
    /// test that compares two `t!` results has to hold the same lock they do or it can read
    /// two different locales.
    fn locale_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
        guard
    }

    /// Hands `window` one worker event exactly as `poll_job` would receive it.
    ///
    /// The channel is real: the folds under test are the ones that decide what a finished or
    /// refused job does to the pending rows, and testing them through the same path the
    /// worker uses is what makes the test worth having. `poll_job` clears `rx` after the
    /// event, so the sender staying alive costs nothing.
    fn fold_event(window: &mut WatermarkLibraryWindow, event: LibraryEvent) {
        let (tx, rx) = mpsc::channel::<LibraryEvent>();
        tx.send(event).expect("the receiver is alive");
        window.rx = Some(rx);
        window.poll_job(&egui::Context::default());
    }

    /// One captured crop, as the tool's worker hands it over.
    fn captured(name: &str, measured: Option<[f32; 3]>) -> CapturedCrop {
        CapturedCrop {
            path: std::env::temp_dir().join(format!("manhwastudio-wm-ui-test-{name}.png")),
            preview: egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 255]),
            measured,
            margins: CropMargins::uniform(4),
        }
    }

    /// Entering one entry's screen and coming back leaves the list exactly as it was, and a
    /// listing refresh that still carries the entry does NOT throw the user out of it.
    #[test]
    fn the_edit_screen_survives_a_refresh_and_the_way_back_restores_the_list() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a"), summary("b")]);
        assert_eq!(window.mode, LibraryPanelMode::List);

        window.run_entry_action(EntryAction::Edit("a".to_string()));
        assert_eq!(window.mode, LibraryPanelMode::Entry("a".to_string()));

        window.fold_listing(vec![summary("a"), summary("b")]);
        assert_eq!(
            window.mode,
            LibraryPanelMode::Entry("a".to_string()),
            "a refresh that still lists the entry must not close its screen"
        );

        assert_eq!(window.leave_entry_mode(), LibraryPanelRequest::Disarm);
        assert_eq!(window.mode, LibraryPanelMode::List);
    }

    /// A crop delete is armed on the first click and acts on the second, and the arm is an
    /// INDEX into the row list on screen — so everything that can invalidate that list must
    /// also drop the arm, or the second click would delete a different crop.
    #[test]
    fn an_armed_crop_delete_never_outlives_the_row_list_it_points_into() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a"), summary("b")]);
        window.run_entry_action(EntryAction::Edit("a".to_string()));

        // Walking to another entry.
        window.run_entry_action(EntryAction::ArmDeleteSample("a".to_string(), 1));
        window.run_entry_action(EntryAction::Edit("b".to_string()));
        assert_eq!(window.confirm_delete_sample, None);

        // Walking back to the card list.
        window.run_entry_action(EntryAction::Edit("a".to_string()));
        window.run_entry_action(EntryAction::ArmDeleteSample("a".to_string(), 1));
        assert_eq!(window.leave_entry_mode(), LibraryPanelRequest::Disarm);
        assert_eq!(window.confirm_delete_sample, None);

        // The entry disappearing from the listing.
        window.run_entry_action(EntryAction::Edit("a".to_string()));
        window.run_entry_action(EntryAction::ArmDeleteSample("a".to_string(), 1));
        window.fold_listing(vec![summary("b")]);
        assert_eq!(window.confirm_delete_sample, None);
    }

    /// A write invalidates the sample list AND the arm: the rows on screen describe crops the
    /// entry no longer has, so they are re-read, and an index into the old list is dropped.
    #[test]
    fn a_write_invalidates_the_sample_list_and_the_arm() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        window.run_entry_action(EntryAction::Edit("a".to_string()));
        window.run_entry_action(EntryAction::ArmDeleteSample("a".to_string(), 0));
        window.samples = Some(EntrySamples {
            entry_id: "a".to_string(),
            revision: 7,
            rows: Vec::new(),
        });
        window.samples_tried = Some(("a".to_string(), 7));

        fold_event(&mut window, LibraryEvent::Changed("written".to_string()));

        assert!(window.samples.is_none(), "the rows are re-read after a write");
        assert!(window.samples_tried.is_none());
        assert_eq!(window.confirm_delete_sample, None);
        assert!(window.changed, "the chapter mode's own copy is stale too");
    }

    /// An entry that disappears from the listing cannot keep its own screen up: the screen
    /// would have a name editor and a sample list for something that no longer exists.
    #[test]
    fn a_deleted_entry_sends_the_panel_back_to_the_list() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a"), summary("b")]);
        window.run_entry_action(EntryAction::Edit("a".to_string()));
        window.fold_listing(vec![summary("b")]);
        assert_eq!(window.mode, LibraryPanelMode::List);
    }

    /// A crop whose ring MEASURED flat and which belongs to an existing entry needs nothing
    /// from the user, so it is committed at once; a crop with no measurement waits, and it
    /// waits until a level is actually stated rather than committing the control's guess.
    ///
    /// The committed-at-once crop still gets a ROW, and keeps it until the worker CONFIRMS the
    /// write: that row is the only place a refusal can be seen from.
    #[test]
    fn a_measured_crop_commits_at_once_and_an_unmeasured_one_waits_for_a_level() {
        let params = SampleParams::default();
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);

        window.accept_captured_crop(
            captured("flat", Some([255.0, 255.0, 255.0])),
            "a",
            params,
            512,
        );
        assert_eq!(
            window.pending.len(),
            1,
            "a measured crop still has a row while the write is in flight"
        );
        assert!(window.rx.is_some(), "a measured crop starts the intake at once");
        assert_eq!(window.committing.len(), 1);
        fold_event(&mut window, LibraryEvent::Changed("written".to_string()));
        assert!(
            window.pending.is_empty(),
            "the confirmed write is what takes the row away"
        );

        window.accept_captured_crop(
            captured("rough", None),
            "a",
            params,
            512,
        );
        assert_eq!(window.pending.len(), 1);
        assert!(window.rx.is_none(), "an unmeasured crop must not be written yet");
        assert!(
            !window.pending_ready("a"),
            "the control's starting guess is not a level the user stated"
        );

        window.commit_pending("a", params, 512);
        assert_eq!(window.pending.len(), 1, "a crop with no level must not be written");
        assert!(window.rx.is_none());

        window.pending[0].level = Color32::from_rgb(10, 20, 30);
        window.pending[0].stated = true;
        assert!(window.pending_ready("a"));
        window.commit_pending("a", params, 512);
        assert!(window.rx.is_some());
        // The row stays until the worker answers — a refused intake keeps its scratch file so
        // the user can correct the level, and a row taken away now would hide it.
        assert_eq!(window.pending.len(), 1);
        assert_eq!(window.committing.len(), 1);
        fold_event(&mut window, LibraryEvent::Changed("written".to_string()));
        assert!(window.pending.is_empty());
    }

    /// `Ok(None)` from the mark renderer is the "no model" case, not a failure: the card falls
    /// back to the stored template and says nothing alarming — unless there is no template
    /// either, which is an EMPTY entry and gets no picture at all rather than an invented one.
    /// `Err` is a message that must reach the user instead of leaving a silently blank card.
    #[test]
    fn an_entry_with_no_model_falls_back_to_its_template() {
        assert_eq!(icon_plan(&Ok(None), true), IconPlan::Template);
        assert_eq!(icon_plan(&Ok(None), false), IconPlan::Empty);
        assert_eq!(
            icon_plan(&Ok(Some(image::RgbaImage::new(1, 1))), true),
            IconPlan::Mark
        );
        assert_eq!(
            icon_plan(&Err("boom".to_string()), true),
            IconPlan::Failed("boom".to_string())
        );
    }

    /// The click that ENDS an eyedropper sampling belongs to the colour control, not to the
    /// canvas — and on that one frame the widget has already cleared `eyedropper_active`, so a
    /// guard that reads only that flag lets the click start a selection drag under the sampled
    /// pixel. Both predicates, or the bug is back.
    #[test]
    fn the_click_that_ends_a_sampling_never_reaches_the_canvas() {
        assert!(
            eyedropper_owns_click(false, true),
            "the terminating frame reports only `consumed`, and it must still be caught"
        );
        assert!(eyedropper_owns_click(true, false));
        assert!(eyedropper_owns_click(true, true));
        assert!(
            !eyedropper_owns_click(false, false),
            "an ordinary frame must leave the canvas alone"
        );
        // A panel with no pending crop has no colour control at all, so it claims nothing.
        assert!(!WatermarkLibraryWindow::default().eyedropper_owns_primary_click());
    }

    /// A card's displayed state and its click must come from ONE answer. An entry a
    /// chapter-discovered mark was saved to is already in the chapter under a `mark-{n}` kind
    /// id: offering «Выбрать» there would push a second kind for one physical mark.
    #[test]
    fn an_entry_already_in_the_chapter_is_neither_selectable_nor_removable() {
        let selected = ["loaded".to_string()];
        let saved = ["saved".to_string()];
        let ctx = LibraryPanelContext {
            sample_params: SampleParams::default(),
            max_side: 512,
            selected_entries: &selected,
            saved_entries: &saved,
            armed: None,
            chapter_busy: false,
        };
        assert_eq!(entry_membership("loaded", &ctx), EntryMembership::Loaded);
        assert_eq!(
            entry_membership("saved", &ctx),
            EntryMembership::SavedFromChapter
        );
        assert_eq!(entry_membership("other", &ctx), EntryMembership::Absent);
    }

    /// A reservation may only outlive the screen that drew its cancel button. Four paths strand
    /// one: hiding the panel, deleting the armed entry, walking to another entry's screen, and
    /// walking back to the card list — which no longer arms anything at all, because «+ новый»
    /// now creates an entry instead of reserving a selection for a nameless draft.
    #[test]
    fn a_reservation_dies_with_the_screen_that_can_cancel_it() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a"), summary("b")]);

        // (a) A hidden panel draws no cancel, so no reservation survives it.
        assert!(!window.is_open());
        window.toggle();
        assert!(
            !window.arm_survives("a"),
            "the card list draws no cancel button: it arms nothing"
        );

        // (c) One entry's screen holds its own reservation and nobody else's.
        window.run_entry_action(EntryAction::Edit("a".to_string()));
        assert!(window.arm_survives("a"));
        assert!(
            !window.arm_survives("b"),
            "one entry's screen cannot hold another entry's reservation"
        );

        // (b) The armed entry is deleted: the listing drops it and the screen goes with it.
        window.fold_listing(vec![summary("b")]);
        assert_eq!(window.mode, LibraryPanelMode::List);
        assert!(!window.arm_survives("a"));
    }

    /// EVERY capture-lane refusal produces a visible row on the entry the drag was aimed at.
    ///
    /// Table-driven on purpose: the guarantee is "no refusal is silent", and a guarantee stated
    /// once per call site is one a future error can quietly skip. Adding a refusal to the lane
    /// without adding it here leaves this test passing on a stale list — so the list is kept
    /// beside `LibraryCaptureRefusal`'s own tags, and every tag has a case.
    #[test]
    fn every_capture_refusal_reaches_the_user_as_a_row() {
        let cases = [
            (
                ReferenceRefusal::TooSmall,
                "the selection has no measurable ring",
            ),
            (ReferenceRefusal::Misaligned, "the mark does not line up"),
            (ReferenceRefusal::Background, "the level is already held"),
            (ReferenceRefusal::Other, "the page could not be decoded"),
        ];
        for (tag, message) in cases {
            let mut window = WatermarkLibraryWindow::default();
            window.fold_listing(vec![summary("a")]);
            window.toggle();
            window.reject_capture("a", &LibraryCaptureRefusal::tagged(message.to_string(), tag));
            assert_eq!(
                window.capture_refusal_reasons("a"),
                vec![message],
                "a {tag:?} refusal must leave a row on the entry it was aimed at"
            );
            assert_eq!(
                window.mode,
                LibraryPanelMode::Entry("a".to_string()),
                "and the panel must be on the screen that draws it"
            );
            assert!(
                window.dirty,
                "and it must ask for the frame that paints it: the dock body already drew"
            );
            assert!(
                window.capture_refusal_reasons("b").is_empty(),
                "a refusal belongs to one entry, not to the panel"
            );
        }
    }

    /// An untagged refusal still gets a row. Only its ADVICE is absent, because no gesture
    /// fixes a page that will not decode.
    #[test]
    fn an_untagged_capture_refusal_is_a_row_without_advice() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        window.reject_capture("a", &LibraryCaptureRefusal::untagged("boom".to_string()));
        assert_eq!(window.capture_refusal_reasons("a"), vec!["boom"]);
        assert!(
            window.capture_refusals[0].advice.is_none(),
            "there is no drag that fixes an undecodable page"
        );
        assert!(
            refusal_advice(ReferenceRefusal::TooSmall).is_some(),
            "while the three actionable tags do carry one"
        );
    }

    /// «+ новый» CREATES an entry and opens it. It arms no canvas selection, so nothing about
    /// making a new mark demands a drag first — which is the whole complaint it answers.
    #[test]
    fn the_new_entry_button_creates_an_entry_and_opens_it() {
        let mut window = WatermarkLibraryWindow::default();
        window.toggle();
        window.run_entry_action(EntryAction::CreateEmpty);
        assert!(
            window.rx.is_some(),
            "creating the entry is disk work and runs on the worker"
        );
        assert!(
            !window.arm_survives("wm-new"),
            "nothing is armed: the list screen draws no cancel button at all"
        );
        // The worker answers the way `save_entry` would have.
        fold_event(
            &mut window,
            LibraryEvent::Created {
                entry_id: "wm-new".to_string(),
                status: "created".to_string(),
            },
        );
        assert_eq!(
            window.mode,
            LibraryPanelMode::Entry("wm-new".to_string()),
            "and the user lands on the screen that takes samples"
        );
        assert!(!window.listed, "the listing is stale and reloads");
        assert!(window.changed, "and the chapter mode's own copy is stale too");
        // Only once it is on screen does arming the canvas mean anything.
        window.fold_listing(vec![summary("wm-new")]);
        assert!(window.arm_survives("wm-new"));
    }

    /// An EMPTY entry renders on both screens without a model and without a template.
    ///
    /// The card cannot fall back to `template.png` — there is none — so it must not try, and
    /// the report must say what the entry is rather than leave the user with "not enough
    /// samples" about a mark the entry has never been given.
    #[test]
    fn an_empty_entry_renders_without_a_model_and_without_a_template() {
        let mut empty = summary("wm-empty");
        empty.has_template = false;
        empty.width = 0;
        empty.height = 0;
        empty.samples = 0;
        empty.levels = Vec::new();
        empty.verdict = "not_enough_samples".to_string();
        empty.fit_method = None;

        // The card: no picture, and explicitly not a failure to report.
        assert_eq!(icon_plan(&Ok(None), empty.has_template), IconPlan::Empty);
        let render = render_entry_icon("wm-empty".to_string(), 0, false);
        assert!(render.image.is_none() && render.error.is_none());

        // The report: the entry's own wording, and the verdict it really has.
        let warnings = entry_warnings(&empty);
        assert!(warnings.is_empty, "no template means no mark yet");
        assert!(warnings.no_model, "and no model either");
        assert_eq!(warnings.partial_rings, 0);
        let line = entry_verdict_line(&warnings, &empty);
        assert_ne!(
            line.text, "cleaning.tools.watermark.chapter.library_verdict_unknown",
            "an empty entry's verdict is known, not unrenderable"
        );

        // The edit screen reaches it: the mode is legal and the panel does not bounce out.
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![empty.clone()]);
        window.run_entry_action(EntryAction::Edit("wm-empty".to_string()));
        assert_eq!(window.mode, LibraryPanelMode::Entry("wm-empty".to_string()));
        window.toggle();
        assert!(
            window.arm_survives("wm-empty"),
            "and its screen can arm a selection, which is how it stops being empty"
        );
    }

    /// A refused intake keeps its scratch files on purpose, so the rows behind them must
    /// still be on screen: a row dropped before the worker answered leaves the user's only
    /// copy of that selection unreachable from every screen.
    #[test]
    fn a_refused_intake_leaves_its_crops_where_the_user_can_fix_them() {
        let params = SampleParams::default();
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        window.accept_captured_crop(
            captured("refused", None),
            "a",
            params,
            512,
        );
        window.pending[0].level = Color32::from_rgb(10, 20, 30);
        window.pending[0].stated = true;
        window.commit_pending("a", params, 512);
        assert_eq!(
            window.pending.len(),
            1,
            "the row stays until the worker answers"
        );
        assert_eq!(window.committing.len(), 1);

        fold_event(&mut window, LibraryEvent::failed("no".to_string()));
        assert_eq!(
            window.pending.len(),
            1,
            "a refused intake must not take the crop with it"
        );
        assert!(window.committing.is_empty());
        let refusal = window.pending[0]
            .refusal
            .as_ref()
            .expect("the row must SAY it was refused, not just survive");
        assert_eq!(refusal.reason, "no");
        assert!(
            refusal.advice.is_none(),
            "an untagged refusal has no gesture that fixes it"
        );

        // The same commit, answered this time: the entry now owns the crop, so the row goes.
        window.commit_pending("a", params, 512);
        assert_eq!(window.committing.len(), 1);
        fold_event(&mut window, LibraryEvent::Changed("written".to_string()));
        assert!(window.pending.is_empty());
        assert!(window.committing.is_empty());
    }

    /// THE SECOND HALF OF THE BUG: a crop that measures flat against an EXISTING entry is
    /// committed the moment it is captured, and when the intake refuses it the user must still
    /// see a row saying so — with an instruction, not only the engine's numbers.
    ///
    /// Before this, the fast path pushed no row at all: a refusal produced no row, no
    /// thumbnail and no sample, and the only trace was a grey `ui.small` line at the very top
    /// of the panel, far from the sample list the drag was aimed at.
    ///
    /// The three tags are the three refusals a re-drag actually hits, proven against real
    /// images in `watermark_entry`'s `the_three_re_drag_refusals_carry_their_tags`.
    #[test]
    fn a_refused_flat_white_capture_leaves_a_row_that_says_what_to_do() {
        let _guard = locale_guard();
        let params = SampleParams::default();
        for (refusal, advice_key) in [
            (
                ReferenceRefusal::TooSmall,
                "cleaning.tools.watermark.chapter.library_refused_too_small_advice",
            ),
            (
                ReferenceRefusal::Misaligned,
                "cleaning.tools.watermark.chapter.library_refused_align_advice",
            ),
            (
                ReferenceRefusal::Background,
                "cleaning.tools.watermark.chapter.library_refused_background_advice",
            ),
        ] {
            let mut window = WatermarkLibraryWindow::default();
            window.fold_listing(vec![summary("a")]);
            // A crop on flat white: measured, aimed at an existing entry — the fast path.
            window.accept_captured_crop(
                captured("flat-refused", Some([255.0, 255.0, 255.0])),
                "a",
                params,
                512,
            );
            assert_eq!(
                window.pending.len(),
                1,
                "the fast path must leave a row for the answer to land on"
            );
            assert_eq!(window.committing.len(), 1);

            fold_event(
                &mut window,
                LibraryEvent::Failed {
                    message: "engine said no".to_string(),
                    refusal,
                },
            );

            assert_eq!(
                window.pending.len(),
                1,
                "a refused capture must stay visible where the user aimed it"
            );
            let row = window.pending[0]
                .refusal
                .as_ref()
                .expect("the row must carry the refusal, not merely survive it");
            assert_eq!(row.reason, "engine said no");
            let advice = row
                .advice
                .as_deref()
                .expect("a re-drag refusal must say which drag would have worked");
            assert_ne!(
                advice, advice_key,
                "the advice must exist in the catalog, not fall back to its own key"
            );
            assert!(
                window.status.is_some(),
                "the panel's own status line still reports it too"
            );
            // The row is still discardable and still backed by its scratch file, so the user
            // can drop it or fix the level and commit again.
            assert_eq!(window.pending[0].target, "a");
            assert!(window.committing.is_empty());
        }
    }

    /// A mutation that is NOT an intake must never be mistaken for the commit whose rows are
    /// waiting: a rename finishing while crops sit in the panel would otherwise erase them.
    #[test]
    fn a_rename_does_not_consume_the_pending_crops() {
        let params = SampleParams::default();
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        window.accept_captured_crop(
            captured("kept", None),
            "a",
            params,
            512,
        );
        window.run_entry_action(EntryAction::Rename("a".to_string(), "b".to_string()));
        assert!(window.committing.is_empty(), "a rename owns no crops");
        fold_event(&mut window, LibraryEvent::Changed("renamed".to_string()));
        assert_eq!(window.pending.len(), 1);
    }

    /// A capture that arrives while the panel's single channel is busy is QUEUED, never lost:
    /// the crop is a selection the user already made and its scratch file is the only copy.
    #[test]
    fn a_capture_that_arrives_during_another_job_is_written_when_the_channel_frees() {
        let params = SampleParams::default();
        let ctx = egui::Context::default();
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        // A listing is in flight, exactly as it would be after «Обновить список».
        window.request_list();
        assert!(window.rx.is_some());

        window.accept_captured_crop(
            captured("busy", Some([255.0, 255.0, 255.0])),
            "a",
            params,
            512,
        );
        assert_eq!(window.deferred.len(), 1, "the crop is held, not dropped");
        assert_eq!(
            window.pending.len(),
            1,
            "the crop has a row from the moment it is captured, queued job or not"
        );

        window.finish_job();
        window.poll(&ctx);
        assert!(window.deferred.is_empty());
        assert!(
            window.rx.is_some(),
            "the queued intake starts as soon as the channel frees"
        );
        window.finish_job();
    }

    /// Every KNOWN verdict has a line of its own. Reporting one through the unknown-verdict
    /// fallback tells the user their build is out of date about an answer the chapter mode
    /// renders correctly two screens away.
    #[test]
    fn every_known_verdict_has_its_own_line() {
        let _guard = locale_guard();
        let mut entry = summary("a");
        entry.verdict = "deposit_unavailable".to_string();
        entry.samples = 3;
        entry.spread = 4.0;
        let line = entry_verdict_line(&entry_warnings(&entry), &entry);
        assert_eq!(
            line.text,
            tf!(
                "cleaning.tools.watermark.chapter.verdict_deposit_unavailable",
                samples = 3usize,
                spread = "4"
            ),
            "a stored `deposit_unavailable` is a verdict this build knows"
        );

        // `Underdetermined` has no stored tag today, so it is reachable only by handing the
        // line the verdict directly — which is exactly how a future writer would reach it.
        let warnings = EntryWarnings {
            is_empty: false,
            partial_rings: 0,
            conditioning: Some(ModelConditioning::Underdetermined {
                underdetermined_pixels: 7,
                total_pixels: 64,
                worst_pixel_spread: 2.0,
                required: 12.0,
                levels: vec![250.0],
            }),
            no_model: false,
            clamped_pixels: 0,
            footprint_pixels: 64,
            clamped_share: 0.0,
            samples_disagree: false,
            manual_backgrounds: Vec::new(),
            rests_on_assertion: false,
        };
        let line = entry_verdict_line(&warnings, &entry);
        assert_eq!(
            line.text,
            tf!(
                "cleaning.tools.watermark.chapter.verdict_underdetermined",
                pixels = 7usize,
                total = 64usize,
                spread = "2",
                required = "12"
            )
        );

        let mut unknown = summary("b");
        unknown.verdict = "from_a_newer_build".to_string();
        assert_eq!(
            entry_verdict_line(&entry_warnings(&unknown), &unknown).text,
            tf!(
                "cleaning.tools.watermark.chapter.library_verdict_unknown",
                verdict = "from_a_newer_build"
            ),
            "only a tag nothing maps reaches the unknown-verdict line"
        );
    }

    /// The correctness obligation of this screen: an entry whose calibration rests on a
    /// HAND-STATED background may never be given the wording that claims a measurement,
    /// whatever its verdict tag says.
    ///
    /// BOTH wording paths are checked, and that is the point of the test. The card list is
    /// only half the feature: the same entry, loaded into the chapter, is described again by
    /// `describe_conditioning` in the region editor, and that half used to print «Отпечаток
    /// измерен точно» for an entry whose card warned about a hand-stated background. A test
    /// that covers one screen gives false confidence about the obligation.
    #[test]
    fn an_asserted_background_is_never_reported_as_measured() {
        let _guard = locale_guard();
        for verdict in ["separable", "deposit_exact"] {
            let mut entry = summary("a");
            entry.verdict = verdict.to_string();
            entry.levels = vec![0.0, 255.0];

            let measured = entry_verdict_line(&entry_warnings(&entry), &entry);
            entry.manual_backgrounds = vec![ManualBackgroundRef {
                file: "samples/1.png".to_string(),
                level: [255.0, 255.0, 255.0],
            }];
            entry.format = 2;
            let warnings = entry_warnings(&entry);
            assert!(warnings.rests_on_assertion);
            let asserted = entry_verdict_line(&warnings, &entry);

            assert_ne!(
                asserted.text, measured.text,
                "{verdict}: an asserted background must change the wording"
            );
            assert_ne!(
                asserted.text,
                t!("cleaning.tools.watermark.chapter.verdict_separable").to_string()
            );
            assert_ne!(
                asserted.text,
                t!("cleaning.tools.watermark.chapter.verdict_deposit_exact").to_string()
            );
            assert_eq!(
                asserted.color,
                ms_theme::status::WARNING,
                "{verdict}: an asserted background is never the affirmative colour"
            );

            // The OTHER half: the same entry described by the region editor, which reads the
            // assertion off the fitted model's provenance instead of the entry's warnings.
            let conditioning = warnings
                .conditioning
                .as_ref()
                .expect("both tags are verdicts this build knows");
            let editor_measured = describe_conditioning(conditioning, false);
            let editor_asserted = describe_conditioning(conditioning, true);
            assert_ne!(
                editor_asserted[0], editor_measured[0],
                "{verdict}: the region editor must change its wording too"
            );
            assert_ne!(
                editor_asserted[0],
                t!("cleaning.tools.watermark.chapter.verdict_separable").to_string()
            );
            assert_ne!(
                editor_asserted[0],
                t!("cleaning.tools.watermark.chapter.verdict_deposit_exact").to_string()
            );
            assert_eq!(
                editor_asserted[0], asserted.text,
                "{verdict}: the two screens must say the SAME thing about one entry"
            );
        }
    }

    /// «Не хватает образцов» is a KNOWN verdict and gets its own line; routing it through the
    /// unknown-verdict fallback would tell the user their build is out of date.
    #[test]
    fn not_enough_samples_has_its_own_line() {
        let _guard = locale_guard();
        let mut entry = summary("a");
        entry.verdict = "not_enough_samples".to_string();
        entry.samples = 1;
        let line = entry_verdict_line(&entry_warnings(&entry), &entry);
        assert_eq!(
            line.text,
            tf!(
                "cleaning.tools.watermark.chapter.verdict_not_enough",
                have = 1usize,
                need = 2usize
            )
        );

        let mut unknown = summary("b");
        unknown.verdict = "from_a_newer_build".to_string();
        let unknown_line = entry_verdict_line(&entry_warnings(&unknown), &unknown);
        assert_ne!(unknown_line.text, line.text);
        assert_eq!(
            unknown_line.text,
            tf!(
                "cleaning.tools.watermark.chapter.library_verdict_unknown",
                verdict = "from_a_newer_build"
            ),
            "an unknown tag is reported literally, never squeezed into a known wording"
        );
    }

    /// An icon is keyed by the entry's revision, so an entry that gained a sample is rendered
    /// again instead of showing the mark it used to have.
    #[test]
    fn a_changed_entry_is_rendered_again() {
        let mut window = WatermarkLibraryWindow::default();
        window.fold_listing(vec![summary("a")]);
        window.icons_tried.insert("a".to_string(), 100);
        window.request_icons();
        assert!(window.rx.is_none(), "an icon already tried at this revision is not re-rendered");

        let mut moved = summary("a");
        moved.updated_unix = 200;
        window.fold_listing(vec![moved]);
        window.request_icons();
        assert!(window.rx.is_some(), "a changed entry must be rendered again");
        window.finish_job();
    }
}
