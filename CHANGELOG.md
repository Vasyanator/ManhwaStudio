# ManhwaStudio — Changelog

Released blocks (`## N — YYYY-MM-DD`) are listed oldest first, newest release last; new entries go under "Unreleased" (and "Не выпущено" in the Russian half).

## 325 — 2026-10-18

### New features

#### AI region editing (Cleaning)

- **New AI region editor** — AI editing now works in a frame drawn directly on the page strip (resize handles, drag strip, mask and result layers, frame size shown, "Compare" button that hides the result to check the pixels underneath) instead of a separate window. Lama, Lama MPE and SDXL are no longer standalone tools but engines of this editor. Every mask-based engine can generate the mask automatically (ComicTextDetector, PaddleOCR, Surya, watermark detection), and the mask stays editable. A mark brush paints hints that are passed to the model as a transparent layer, drawn onto the image, or sent as a separate reference image, depending on what the model accepts.
- **FLUX.2 Klein (4B and 9B)** — local region editing with a text prompt on the AI backend: automatic weight download, per-component status (text encoder, transformer, VAE) with manual load/unload, VRAM/RAM forecast, a no-mask automatic mode, and an option to limit prompt attention to the painted mask. Single-file checkpoints are streamed tensor by tensor, which cuts the host RAM peak dramatically.
- **FLUX.3 Klein** — a new region-editing engine: caption plus up to ten reference images (the region itself is reference #0), choice of weight format (bf16, fp8r, nvfp4), and an in-panel download of the gated Hugging Face repository with access check, token management, size estimate and cancel. The text encoder can be kept on the CPU ("Encoder on CPU", on by default for both Klein models).
- **Automatic colour matching after AI generation** — results of FLUX.2/FLUX.3 Klein are colour-matched to the surrounding page; a local match (strength and scale in the Advanced panel) keeps large patches from drifting toward the scene's average tone and ignores foreign surfaces.
- **Large-model manager** — the AI backend places large models by two user-steerable axes, "Video memory" (max / phase / half / min) and "RAM" (keep / run / min), each with an automatic mode and hard byte caps; FLUX.2 and FLUX.3 Klein run on it. The AI backend settings got a large-model section.
- **Cloud image editing — "AI editing (API)" (experimental)** — region edits through cloud image-edit services (OpenAI, Gemini, BFL FLUX, Qwen/DashScope, Ideogram, Recraft, Runway, Kling, fal, Replicate and others). The result always matches the region size exactly (integer-factor scaling only) and is composited only inside the mask; reference images are sent where the model supports them. Cancelling a run also cancels the job on the provider side.

#### Cleaning

- **Patch tool** — a Photoshop-style patch: select an area with a lasso or rectangle and drag it onto a clean source area; pixels are copied with colour adaptation to the destination edges (none / linear / multiplicative) and feathering, as one undo step. Available in both the Cleaning tab and the PS editor.
- **Watermark removal (experimental)** — a new Cleaning tool with three modes: mask only, network clean, and chapter-wide exact subtraction of a watermark across all its occurrences, backed by a named watermark library (template + auto-matching; selection via Shift+LMB, refused selections explain why).

#### Typing

- **Custom kerning pairs** — per-font kerning pairs (in thousandths of an em) edited in the font properties window, with an adjustable preview size. The font file is not modified; the authored value replaces the font's own kerning for that pair.
- **Hybrid custom fonts** — a custom font is a recipe that blends 2–4 installed fonts with one of five blend algorithms (including skeleton matching) and optionally separate weights for shape, spacing and kerning. It is compiled into a real font in memory and cached on disk, the hybrid editor has a live preview, and a layer whose source font is missing is blocked instead of silently substituted.
- **Movable dock panels** — the panels of the Typing tab (and later of Cleaning, Translation and the PS editor) can be moved, re-docked and detached into their own windows; the page ribbon is a panel tab too. Panel sections remember whether they are open.
- **Character table** — a symbol picker in the EDIT panel inserts symbols at the caret, wrapping them in a font tag only when the chosen font differs from the base one; two favourite lists (per title and global).
- **Colour presets per title** — 20 colour cells per title, available in every colour picker of the text panel (text and effect colours).
- **Local presets** — a set of text parameters can be saved as a local preset.
- **Searchable font list** — the font selector has a search field.
- **Duplicate text layer (Ctrl+D)** — also available from the layer context menu.
- **PDF export** — lossless PDF export, with optional re-pagination into pages of a fixed height.
- **Font cards from PSD** — font group settings can read a PSD whose text layers are set in the fonts they name, matching each name to an installed font.
- **Ellipsis options** — "Replace ellipsis with three dots" at render time and forced removal of the font's ellipsis glyph.

#### Translation and OCR

- **Baberu OCR** — offline Japanese/Chinese/English manga OCR on the built-in ONNX runtime, no Torch needed (falls back to the AI backend). Korean is not supported by this model.
- **Hint bubbles** — a one-line author note anchored to a point on the page (hotkey H); it can be added to the translation prompt and optionally shown outside the Translation tab.
- **Hangul keyboard** — a floating jamo keyboard, opened from the context menu of a bubble's text fields, composes Hangul syllables and inserts them at the caret.
- **All-caps OCR correction** — English and Russian OCR results written in all caps are converted to sentence case (the English "I" stays capitalised); applied on every OCR route.

#### Page Manager

- **Stitch, split and crop pages** — join two or more pages into one (row or column, alignment, fit modes, background), cut a page into parts along horizontal or vertical lines, crop a page. All of them are journalled, crash-safe operations like the other page operations.

#### PS editor

- **Colour correction (beta)** — a "Correction" panel with brightness/contrast levelling. It affects only the on-screen picture, never the layer pixels or the saved project.

#### Studio and interface

- **Open images without a project** — "Open image" in the launcher, the command line or the system "Open with" menu opens one or several images (natural name order) as one session, with progress and cancel; unreadable files are skipped and listed. Save writes back only the pages that actually changed, as flattened images (PNG, JPEG, WebP); Save As or pages without a writable source go to a chosen folder. Images sent to a running program open in the launcher immediately or after a confirmation in the studio.
- **Interface scale** — General setting "UI scale" 50–200 %, for the studio and the launcher.
- **Rebindable hotkeys everywhere** — every key, modifier, wheel and mouse-button shortcut in Translation, Cleaning, Typing and on the canvases is a named action with any number of alternatives, key sequences and modifier rules. Settings › Hotkeys was rewritten: grouped by area, add/reassign/remove/reset, capture of keys, mouse buttons and sequences, conflict list, search. On-screen hints always show the current bindings. Existing settings are migrated once.
- **"Secondary windows" setting** — Automatic / Separate windows / Inside the main window.

#### Launcher and installer

- **Settings warnings** — the launcher settings list problems with the AI setup: the backend cannot start (missing Python, outdated backend files), the saved device is not found, the ONNX build is unavailable or falls back to the CPU, the last ONNX load crashed, the projects folder is missing.
- **System registration tab** — shows and repairs the program's OS records (Windows: Uninstall, App Paths, "Open with" and shortcuts; Linux: desktop entry), with administrator-rights hints and a confirmation before touching records that belong to another copy.
- **Replace installed copy / run standalone (Windows)** — when another installed copy exists, the install window offers to replace it with this one (only the executable is replaced, the Python environment stays) or to run this copy standalone.
- **Third-party AI model downloader** — Baberu and PaddleOCR-VL files are downloaded from pinned sources, verified by size and SHA-256, resumable and cancellable.
- **Run from source** — `run-dev` launchers for Linux, Windows and macOS update the sources from git, set up Rust and start the program.
- **Beginner guide** — a new wiki page with 38 screenshots.

#### Touch and Android (experimental, device testing pending)

- **Finger gestures on canvases** — one finger pans, two fingers pan and zoom, a long press opens the context menu or starts a selection; axis lock and inertia. Works on the page canvases, the PS editor and tool frames; in Typing a tap selects a text layer and a long press opens its menu. Mouse and pen keep tool behaviour. Setting General › "Touchscreen gestures" (on by default on Android, hidden on Windows).
- **Android port** — an APK build (arm64-v8a, Android 12+) with system file pickers, system insets kept out of the UI, a hardware keyboard, the built-in ONNX runtime (CPU and XNNPACK, nothing downloaded), AI API keys and the Hugging Face token stored in the Android Keystore, and a finger-drawn OCR frame. The AI backend and screen capture are not available on Android.

### Improvements

- **One main window** — the launcher and the studio are screens of the same window; opening a chapter shows a loading screen and closing it a saving screen, with all the heavy work off the GUI thread. New Project, PSD import, Wiki and batch processing can open as embedded windows.
- **SQLite storage by default** — documents are stored in a SQLite store (`.db`) in the default "prod" mode; "dev" mode keeps JSON, and a global conversion job switches between them.
- **Single save manager** — one manager writes the session draft every 3 minutes or after 30 actions, with far fewer disk writes.
- **AI API OCR and translation** — custom OpenAI- and Anthropic-compatible servers (base URL, key optional); the key is checked on entry, with account balance and limits; image-input support shown as supported / not supported / unknown; live "Thinking… / Answering…" status with a character counter and a Stop button; tolerant parsing of model answers (JSON extracted from prose, small damage repaired, partial results kept, only missing IDs re-requested); suspicious translations are marked as warnings instead of failing; optional "Strict JSON format".
- **PaddleOCR-VL** — choice of PaddleOCR-VL 1.6, 1.5 and a manga variant.
- **Text detection on large pages** — scale and tiling are planned per engine (CTD, PaddleOCR, Surya) and the plan is shown in the panel; oversize pages are refused with a message.
- **Resilient downloads** — AI models, the ONNX runtime and the installer use one download engine: resume, retry with backoff (visible in the UI), cancel, size and SHA-256 checks. The launcher's quick download can be cancelled.
- **File dialogs no longer block the window**; system yes/no prompts are replaced by in-app dialogs.
- **Faster model loading on AMD (ROCm)** — weights are staged through anonymous memory before the GPU copy, avoiding a driver stall.
- **Local AI unavailable state** — with `--no-ai` or on platforms without local AI, OCR and MT panels stay visible and show the reason; cloud AI API and non-AI machine translation keep working under `--no-ai`.
- **Quick download: 19 new sites** — Dandadan, Dankefuerslesen, Dynasty Scans, Hentai2Read, Hiperdex, Kaliscan, Komikcast, Kuaikan, Mangafreak, Manganelo, Mangapark, Mangaread, Mangataro, Naver, Rawkuma, Senmanga, TCB Scans, WeebCentral, Weebdex.
- **PSD import** — PSB files; plain images in the same selection, folder or archive are imported as pages; a layer can be overlaid onto the page's clean; previews on a transparency checkerboard.
- **Typing effects** — a soft glow variant (blur radius, bias, knee, per-side expansion); gradient area mode and colour tolerance; forced bold usable as an inline tag and able to thin glyphs.
- **Typing panel** — hanging punctuation as a 0–100 % strength slider; hex colour input (#rrggbb / #rrggbbaa); the on-canvas text field draws at the render size; centering-assist settings are saved per layer; global presets got separate create/rename/delete and an unsaved-changes warning; advanced text forms were rebuilt; font groups can show font names or titles and list members as a table; the bundled UI font stack is offered as a typing font.
- **PS editor** — Photoshop-like lasso and brush; painting on the clean layer and hiding the base layers; pixel grid and smoothing toggles; dock panels.
- **Page Manager** — clean cards ("clean ✓" / "Problem", two-step detach, "clean without page" section with attach/delete) and a full-resolution viewer for large images.
- **Cleaning tab** — the status line warns when the page's clean was not loaded because of a size mismatch and when a clean belongs to no page.
- **New Project** — the ribbon width can be dragged, "Display at equal width" option; the batch processing canvas has a fixed grid, zoom-independent nodes and non-blocking file dialogs.
- **Translation** — character name autocomplete also uses names from the chapter's bubbles and service names.
- **Natural file ordering** — case-insensitive, Cyrillic-aware (`p2` before `p10`).
- **Wiki** — numbered lists, blockquotes and image rows render correctly.
- **Backend compatibility** — the "versions do not match" warnings are gone; compatibility is decided only by the protocol version.

### Behaviour changes

- **New version numbering** — versions are now plain build numbers (for example 330) instead of 3.x.y; releases come out regularly, and each release lists only its own changes. Copies with 3.6.0 update to the new numbering automatically.
- Translation panel shortcuts require exact modifiers (Shift+S no longer opens the OCR panel).
- By default only the left mouse button moves bubbles and handles.
- Ctrl+wheel over docked panels no longer zooms the canvas; Ctrl+= zooms around the pointer.
- The UI font always comes from the global font set; a title's `fonts/ui` folder no longer overrides it.
- The ellipsis glyph is removed by default.
- New ribbon defaults: side bubbles compressed for manga, translation status shown as marks, two side-bubble columns allowed.

### Fixes

- **Clean changes reach Typing** — after Ctrl+Z, quick clean or PS-editor edits the Typing canvas could show a stale clean.
- **Typing export** uses the latest PS-editor edits instead of a stale projection.
- **Unsaved changes are no longer lost** — deferred PS-editor layer changes (visibility, opacity, folding) survive closing the studio; bubble edits made during a save-to-project merge keep the session marked unsaved.
- **No GUI freezes on config and dictionary writes** — these writes now run in the background.
- **Glow and stroke** keep the text's transparency; the thin light line between the glow and the outline is gone.
- **Stamp tool** — text contours no longer show through the stamped patch; the sampled pixels are shown.
- **Text layout** — uppercase word wrapping and the no-break tag in the advanced form; horizontal alignment along custom lines and shapes; the custom layout's minimum distance to a character; glyph height in vertical text; centering assist ignores punctuation.
- **Text entry and layers** — the create field no longer shifts the font after a selection; moving a layer with arrow keys updates its mask; layers no longer blink when one is deleted; old 2.x text layers are read correctly.
- **Canvas** — the horizontal scroll limit; the ribbon no longer jumps to the right after a reload or shifts when the first bubble is created or the last one deleted.
- **AI API OCR** respects "join lines"; read errors for notes, characters and terms are shown instead of being ignored; backend OCR route checks no longer falsely refuse on the first check.
- **Downloads** — WEBTOONS and MangaDex quick download; deep interception (CloakBrowser) page order, mixed `<img>`/`<canvas>` pages and duplicates; the advanced downloader can be reopened after its browser was closed by hand.
- **Page Manager** — more space between a clean card and the next page row.

### Technical

- The application was split into 44 `ms-*` crates; egui/eframe updated to 0.36.2.
- Backend protocol version 16; the Python backend was reorganised into domain packages.
- The launcher's export page warns that chapter export uses the program's own format.
- Updates and fresh installs verify every downloaded file against the release's SHA-256 checksums before using it.

## Unreleased

### New features
- **«Fill enclosed» in the area editor** — a button next to «Generate mask» fills everything the selected mask layer completely surrounds: outline an area with the brush and fill it in one click. The edge of the area does not count as a boundary.
- **Text mask and blocks from a recognition selection** — PaddleOCR and Surya have a new «Create mask and text blocks» parameter in the Text recognition panel: with it on, selecting an area for recognition also finds the text in it with the same engine and puts the text detector's mask and text blocks there, replacing the old ones inside the selection only. While the parameter is on, the mask and text blocks stay visible on the page, as with the text detector panel open.
- **NeuroLate recognition and text detection** — the cloud service NeuroLate is a new engine in the Text recognition panel and a new mode of the text detector in the Translation tab. Enter the NeuroLate API key once (it is kept in the system credential store) and see how much of its limit is left. Choose the source language (any of the 58 languages NeuroLate supports, with a search in the list) and the comic type (webtoon or manga), or leave both on «Auto». With «Create mask and text blocks» on, one recognition request also returns the text mask and text blocks of the selection, at no extra charge. In the text detector, «Recognize text right away» reads the text of every detected block in the same request and creates a bubble with it for each block, as «Recognize blocks» would (the text of every block is charged too). NeuroLate is a paid service: every request spends tokens from your NeuroLate balance, and a failed request is never resent automatically.
- **PyTorch for AMD Radeon on Windows and Linux** — the installer and the PyTorch section of the launcher now offer a GPU PyTorch for AMD graphics cards (ROCm) on Windows too, not only on Linux. The card's architecture is detected automatically and can be changed by hand; the download size and the required driver version are shown before installing. No separate ROCm or HIP SDK installation is needed. On Linux, ROCm PyTorch is now offered on any machine with a supported card, even without a system ROCm installed.
- **PyTorch build in the AI backend settings** — the CUDA/ROCm diagnostics show which PyTorch build is installed and which GPU it sees, and warn when the installed PyTorch has no kernels for your card.
- **Machine translation through your own browser** — Google, DeepL and Yandex can each be switched to «Through my browser» in the machine translation panel: the translation is then done on the service's regular website in your own Chrome, Edge, Yandex Browser or Brave by the ManhwaStudio browser extension, which helps when a service limits or blocks the program's requests. The panel installs and updates the extension in one click and shows the three steps to load it in the browser (open the extensions page, turn on Developer mode, «Load unpacked» this folder), whether a browser is connected and which one. If the website shows a check, the panel asks you to solve it in its tab and the translation continues; «Stop» still works. Only one running ManhwaStudio window works with the extension at a time; another window says so. «Built-in requests» stays the default.
- **Composition presets in the Translation tab** — the «Parameters» tab of the Composition panel has a preset picker: save the current parameters (sorting and the MiniJinja template included) as a named preset, then rename, update or delete it. Presets are shared by all titles, the selected preset is remembered per title, and you see at once when the parameters differ from the preset. Two built-in presets ship: «Translation export» and «AI translation»; they can be edited and reset to their factory values, and their names follow the interface language.
- **Title type setting** — Settings › Ribbon and bubbles has a new «Title type» row (Pages / Webtoon), separate from the layout preset. NeuroLate's automatic comic type and the default re-pagination of the PDF export follow it, and a change applies at once, without reopening the title.
- **Compact FLUX.2 klein weights (GGUF)** — in the area AI editor, choose Q8_0 / Q6_K / Q5_K_M / Q4_K_M separately for the transformer and for the text encoder, for klein 4B and 9B (the 9B uncensored encoder too, in Q8_0 / Q6_K / Q4_K_M): for 4B from about 2.6 GB + 2.5 GB instead of 7.8 GB + 8 GB on disk and in memory, at the cost of slower generation steps. The memory and speed panel shows both choices. GGUF files picked by hand from another folder work too, as long as the regular 4B model is installed.

### Improvements
- **Stop for the text detector** — a running text detection over several pages can now be stopped: the page being processed is finished and applied, then the run stops.
- **Post-processing of FLUX klein results** — in the area editor, edge feathering and colour matching to the original now live in a separate «Post-processing» section and apply to the shown result instantly, without regenerating it. Colour marks no longer bleed into the edge of the inserted result. Colours are now matched even when nothing is painted (the whole area is regenerated): the edges of the area serve as the reference.
- **«Hide frame» in the area editor** — a toggle next to «Compare» hides the frame, its handles, the masks and the status line while a result is waiting to be applied, so you can check how the edges of the area join the page.
- **System information in the launcher** — the «System information» tab in the launcher settings is redesigned and now works on Windows, Linux, macOS and Android: the system (OS, processor, cores, total and free memory), every graphics card with its type, video memory, architecture and driver, and what the AI can use on this machine — the built-in AI acceleration, the PyTorch build the installer would offer and the PyTorch the AI backend is running. The list of ROCm 7.2 architectures is gone. «Copy report» puts the whole page into the clipboard. The check no longer starts PowerShell on Windows and no longer hangs if a system tool stops responding.
- **Translation tools as dockable panels** — the «Tools» bar of the Translation tab and each of its tools (Bubbles, Text recognition, Composition, Machine/AI translation, Bulk text detector) are now dockable panels like the rest of the tab. Several tools can be open at once: their buttons and hotkeys switch each tool on and off on its own, and the buttons show which tools are on. By default the tools open as tabs of one panel above the bar; drag them anywhere, and they come back to where you left them. The open tools are remembered between launches. The tool window no longer floats over the panels.
- **Machine translation respects service limits** — Google, DeepL and Yandex requests are now paced to each service's limits, also when the Translation and Cleaning tabs translate at the same time. When a service asks to slow down, translation waits it out by itself: the panel shows what it is waiting for and how long, and «Stop» ends the wait at once. When a service blocks requests for longer, the run stops and says when to try again; the remaining bubbles stay untranslated. A temporarily blocked Google switches to a backup address and returns to the main one later, and very long texts are sent to Google in parts. DeepL and Yandex translate several bubbles per request. DeepL follows its real request allowance (a few requests at once, then about one per minute) and stops with a «try again in N min» message instead of retrying into a long block; runs no longer spend seconds opening a new connection to DeepL. A stalled connection to DeepL or Yandex no longer hangs the translation.
- **Panel tabs wrap onto new rows** — when a panel is too narrow for all its tab names in one row, the tab names continue on the next row instead of running off the panel. A panel can no longer be made narrower than its longest tab name.
- **Lighter editor when idle** — the editor no longer redraws itself continuously while nothing happens: after fast scrolling in Cleaning or Typing, with the Typing mask panel or text editors open, with a sticky status message, or while pages load. Large chapters scroll more smoothly in Translation, Cleaning and Typing, and detached panel windows of other tabs no longer slow the main window down while you work in a tab without panels. Typing no longer briefly freezes when you scroll to a page with Photoshop layers for the first time: its layers appear a moment later.
- **Layout presets** — the preset in Settings › Ribbon and bubbles now sets the whole page and bubble layout (separate pages, spacing and margins, aside bubble width, scale, side, stretch and second column, default bubble types, on-top focus) and is remembered for each title; changing a value marks the preset as unsaved instead of switching it to «Custom». Create, rename, save and delete your own presets (shared by all titles); the built-in «Pages» and «Webtoon» can be edited and reset to their factory values, and new titles use your edited version.
- **Character name placement in the composition** — when «Use character names» is on in the «Parameters» tab of the Composition panel, choose whether the name goes before or after the line and set the text placed before and after the name (for example «Name: line» instead of «line - Name»). By default the result is the same as before.
- **FLUX.2 / FLUX.3 klein model loading progress** — reading the model from disk now shows its own bar for each part (transformer, text encoder, VAE), updated five times a second, with the read speed; FLUX.3 klein shows it for the first time, and FLUX.2 klein now also shows the text encoder being read.
- **FLUX.2 klein memory forecast counts the text encoder** — the RAM and VRAM figures now include the text encoder whenever the run needs it or it is already loaded; it is left out only when every prompt is cached and the encoder is not loaded.

### Behaviour changes
- **FLUX.2 klein 4B and 9B are downloaded by the app itself** — pinned files with checksums; a model you already downloaded is checked once and kept, nothing is downloaded again (a 9B transformer saved as a diffusers folder is kept as well). For the gated 9B repositories the panel says exactly what is missing — a Hugging Face token, a rejected token or the terms on the model page — with a link to the page.

### Fixes
- **Saving the text detector's mask** — «Save selection» in the text detector panel of the Translation tab now saves the mask exactly as you see it, including what you painted or erased with the mask brush; before, those edits were lost.
- **AMD graphics detection** — machines with an NVIDIA or Intel card are no longer mistaken for having an AMD card, and the ROCm version on Linux is no longer misread (it could show «1.18»).
- **Google machine translation** — translating bubbles through Google works again; before, every bubble failed with «Invalid request». When Google does refuse a request, the error now says why (temporarily blocked, too many requests, network problem) and what to do.
- **Docking a wider panel to a narrower one** — a panel docked under, above or beside a shorter neighbour can now be placed anywhere along it as long as one side stays within the other; before, a wider panel always snapped flush with the narrower one's left (or top) edge. Existing panel arrangements stay where they are.
- **Background work on hidden tabs** — machine translation, text recognition and text detection started in Translation, text-mask loading and quick cleaning in Cleaning, and text rendering in Typing now keep running and reach the other tabs while you are on another tab; before, their results waited until you returned to the tab that started them.
- **A title's layout no longer depends on other titles** — the aside bubble width, compaction, side, second column and stretch are now saved with each title instead of being shared by all titles, so setting up one title no longer changes the layout of another. Existing titles keep their layout, except that the aside bubble compaction of a page-based or webtoon title goes back to the value of its own type. A title's type no longer silently turns into «Custom» when a setting is changed — that made NeuroLate treat a manga as a webtoon and the PDF export re-paginate a page-based title. The Pages / Webtoon choice when a title is opened for the first time now sets its whole layout, and opened images always use the «Pages» preset.
- **Canvas panel toggles survive Settings changes** — «Show bubbles» and the bubble opacity changed on a canvas panel are no longer reverted when you then change a setting in Settings › Ribbon and bubbles.
- **FLUX.1-Fill works offline** — once FLUX.1-Fill is downloaded, inpainting with it no longer contacts Hugging Face on every run, so it works without internet and a network hiccup no longer makes it fail.
- **Cancelling a model download is immediate** — cancelling a download that the AI backend performs now stops it within about a second even when the connection has stalled; before, it could take up to a minute. A download cut off by a crash or force-quit now continues from the bytes already on disk instead of starting over.
- **AI environment repair** — the installer and the environment check now bring an existing AI environment up to the tested package versions: they install the missing packages FLUX.2/FLUX.3 klein and FLUX Fill need, keep the language-model library on its tested version and fix packages installed at the wrong version.
- **Text detector** — Save no longer deletes or overwrites saved page detections that could not be read, and says how many it kept; edited detected lines now reach Cleaning's quick clean.
- **OCR status** — a recognition that was replaced or lost no longer leaves the status stuck on «detecting».
- **Region editing settings** — SDXL, Lama and cloud image-edit settings are saved once a dial settles instead of on every step, written safely, and the last change is no longer lost when the app closes right after it. When status checks of a cloud edit fail for good, the job is also cancelled at fal, Replicate or Runway. Cancelling with no result says «Processing cancelled.» instead of «result discarded».
- **Quick text clean** — text masks the detector saved at a reduced size are now recognized as belonging to their page.
- **Formula presets** — a failed save is now shown in the panel status line instead of being lost silently, two quick saves can no longer land in the wrong order, and preset names keep spaces exactly as typed.
- **Save and discard status** — the status line updates the moment saving to the project or discarding finishes, without needing to move the mouse.
- **Quick OCR availability** — quick recognition on the Translation canvas follows the AI backend's current state even when all tools are hidden.
- **Touch input** — a touch cancelled by the system (for example by palm rejection) no longer leaves the pointer held down.
- **SDXL error message** — when the SDXL library fails to load, the error now shows the real cause instead of always asking to install it.
- **Right-to-left text spacing** — Hebrew and other right-to-left text no longer runs backwards or misspaces letters with «Metric» or optical kerning; your own kerning pairs now tighten or loosen right-to-left letter pairs by exactly the value entered (a 0 entry no longer shifts letters); faux bold widens right-to-left text instead of squeezing it.
- **Right-to-left lines keep their place** — Hebrew and other right-to-left text whose length changes through custom kerning pairs, «Metric» or optical kerning, or faux bold is now aligned and wrapped by its real width: no gap at the left of a tightened line and no overflow past the left edge of a widened one; line wrapping now also accounts for faux bold.
- **Letter spacing in right-to-left text** — positive letter spacing now spreads Hebrew and other right-to-left text apart (and negative spacing tightens it), instead of doing the opposite.
- **Cleaning brush and stamp** — the clean layer no longer blinks when you finish a brush or stamp stroke, and other pages on screen keep their clean while you paint.
- **Custom kerning pairs and line breaks** — line wrapping and the text-forms search now measure your custom kerning pairs exactly as they are drawn, so a line that fits thanks to tightened pairs is no longer broken early, and pairs drawn by a fallback font or merged into a ligature no longer skew the measurement.
- **Line breaks follow letter spacing** — text with manual letter spacing (in px or %) or «Metric» kerning now wraps where its drawn lines actually end instead of running past the text box, and the text-forms window sizes its options the same way.
- **Python environment repair** — environments set up by older versions no longer end up with two conflicting OpenCV packages: the duplicate is removed and the right one reinstalled; the readiness message now says packages are «missing or outdated».
- **Image bubbles** — pasting an image from the clipboard or choosing an image file for a bubble no longer freezes the window while it is saved, and an interrupted save can no longer leave a damaged file.
- **New Project window** — its dropdowns and the download-threads slider react to the mouse wheel like the rest of the app, without scrolling the panel.

---

# ManhwaStudio — Список изменений

Выпущенные блоки (`## N — ГГГГ-ММ-ДД`) идут от старых к новым, последний выпуск внизу; новые записи добавляются под «Не выпущено».
- **Uncensored text encoder of FLUX.2 klein 9B** — it now encodes prompts with its real positional settings (they were read wrongly from its configuration); prompts cached with it are encoded again once.

## 325 — 2026-10-18

### Новые возможности

#### ИИ-редактирование области (Клин)

- **Новый ИИ-редактор области** — ИИ-редактирование теперь идёт в рамке прямо на ленте страниц (ручки изменения размера, полоса перетаскивания, слои маски и результата, размер рамки, кнопка «Сравнить», скрывающая результат), а не в отдельном окне. Lama, Lama MPE и SDXL больше не отдельные инструменты, а движки этого редактора. Любой движок с маской умеет строить её автоматически (ComicTextDetector, PaddleOCR, Surya, детекция водяных знаков), маску можно править. Кисть пометок рисует подсказки, которые передаются модели прозрачным слоем, поверх изображения или отдельным референсом — в зависимости от того, что модель принимает.
- **FLUX.2 Klein (4B и 9B)** — локальное редактирование области по текстовому запросу на ИИ-бэкенде: автоскачивание весов, состояние каждой части (текстовый энкодер, трансформер, VAE) с ручной загрузкой и выгрузкой, прогноз VRAM/RAM, автоматический режим без маски и режим внимания запроса только к маске. Однофайловые чекпоинты грузятся потоково, по тензору, что резко снижает пик потребления ОЗУ.
- **FLUX.3 Klein** — новый движок редактирования области: описание и до десяти референсов (сама область — референс №0), выбор формата весов (bf16, fp8r, nvfp4), скачивание закрытого репозитория Hugging Face прямо из панели с проверкой доступа, управлением токеном, оценкой размера и отменой. Текстовый энкодер можно держать на CPU («Энкодер на CPU», по умолчанию включено для обеих моделей Klein).
- **Автоподгонка цвета после ИИ** — результат FLUX.2/FLUX.3 Klein подгоняется по цвету к окружению; локальная подгонка (сила и масштаб в панели «Дополнительно») не даёт большим заплаткам уходить в средний тон сцены и игнорирует чужие поверхности.
- **Менеджер больших моделей** — ИИ-бэкенд размещает большие модели по двум осям, которыми управляет пользователь: «Видеопамять» (max / phase / half / min) и «Оперативная память» (keep / run / min), у каждой есть авто-режим и жёсткие лимиты в байтах; на нём работают FLUX.2 и FLUX.3 Klein. В настройках ИИ-бэкенда появился раздел больших моделей.
- **Облачное редактирование — «ИИ редактирование (API)» (экспериментально)** — редактирование области через облачные сервисы (OpenAI, Gemini, BFL FLUX, Qwen/DashScope, Ideogram, Recraft, Runway, Kling, fal, Replicate и другие). Результат всегда точно совпадает по размеру с областью (только целочисленное масштабирование) и накладывается только внутри маски; референсы передаются, если модель их поддерживает. Отмена запуска отменяет и задачу у провайдера.

#### Клин

- **Инструмент «Заплатка»** — заплатка как в Photoshop: выделите область лассо или прямоугольником и перетащите на чистый участок; пиксели копируются с подгонкой цвета к краям (без подгонки / линейная / мультипликативная) и растушёвкой, одним шагом отмены. Есть и во вкладке клина, и в PS-редакторе.
- **Удаление водяных знаков (экспериментально)** — новый инструмент клина с тремя режимами: только маска, очистка нейросетью и точное вычитание водяного знака по всей главе, с именованной библиотекой водяных знаков (шаблон + автоподбор; выделение через Shift+ЛКМ, при отказе объясняется причина).

#### Тайп

- **Кастомные кернинговые пары** — пары кернинга для каждого шрифта (в тысячных долях em) в окне свойств шрифта, с регулируемым размером превью. Файл шрифта не меняется; заданное значение заменяет собственный кернинг шрифта для этой пары.
- **Гибридные кастомные шрифты** — кастомный шрифт — это рецепт, смешивающий 2–4 установленных шрифта одним из пяти алгоритмов (включая сопоставление скелетов), при желании с отдельными весами для формы, интервалов и кернинга. Он собирается в настоящий шрифт в памяти и кэшируется на диске, у гибридного редактора есть живое превью, а слой с отсутствующим исходным шрифтом блокируется, а не получает молчаливую замену.
- **Перемещаемые док-панели** — панели вкладки тайпа (затем и клина, перевода и PS-редактора) можно перемещать, перестыковывать и выносить в отдельные окна; лента страниц тоже стала вкладкой панели. Разделы панелей запоминают, открыты они или свёрнуты.
- **Таблица символов** — в панели EDIT вставляет символы в позицию курсора, оборачивая их в тег шрифта, только если выбранный шрифт отличается от основного; два списка избранного (на тайтл и общий).
- **Цветовые пресеты на тайтл** — 20 ячеек цвета на тайтл в каждом выборе цвета панели текста (цвет текста и эффектов).
- **Локальные пресеты** — набор параметров текста можно сохранить как локальный пресет.
- **Поиск в списке шрифтов** — у выбора шрифта есть поле поиска.
- **Дублирование текстового слоя (Ctrl+D)** — также из контекстного меню слоя.
- **Экспорт в PDF** — без потерь, с возможностью перерезать на страницы фиксированной высоты.
- **Карточки шрифтов из PSD** — в настройках групп шрифтов можно прочитать PSD, где текстовые слои набраны шрифтами, которые они называют; каждое имя сопоставляется установленному шрифту.
- **Настройки многоточия** — «Заменять многоточие тремя точками» при рендере и принудительное удаление глифа многоточия шрифта.

#### Перевод и OCR

- **Baberu OCR** — офлайн-OCR манги для японского, китайского и английского на встроенном ONNX-рантайме, без Torch (запасной путь — ИИ-бэкенд). Корейский эта модель не поддерживает.
- **Пузыри-подсказки** — однострочная заметка автора, привязанная к точке страницы (клавиша H); её можно добавить в промпт перевода и при желании показывать вне вкладки перевода.
- **Корейская клавиатура** — плавающая клавиатура чамо, открывается из контекстного меню текстовых полей пузыря, собирает слоги хангыля и вставляет их в позицию курсора.
- **Исправление капса в OCR** — результаты OCR на английском и русском, набранные капсом, переводятся в обычный регистр предложения (английское «I» остаётся заглавным); работает на всех путях OCR.

#### Менеджер страниц

- **Склейка, разрезание и обрезка страниц** — объединение двух и более страниц в одну (в ряд или столбец, выравнивание, режимы подгонки, фон), разрезание страницы на части по горизонтальным или вертикальным линиям, обрезка. Как и остальные операции со страницами, они журналируются и переживают сбои.

#### PS-редактор

- **Цветокоррекция (бета)** — панель «Коррекция» с выравниванием яркости и контраста. Меняет только изображение на экране, а не пиксели слоёв и не сохранённый проект.

#### Студия и интерфейс

- **Открытие изображений без проекта** — «Открыть изображение» в лаунчере, командная строка или системное «Открыть с помощью» открывают одно или несколько изображений (в естественном порядке имён) одной сессией, с прогрессом и отменой; нечитаемые файлы пропускаются и перечисляются. «Сохранить» записывает обратно только реально изменённые страницы как сведённые изображения (PNG, JPEG, WebP); «Сохранить как» и страницы без доступного для записи источника идут в выбранную папку. Изображения, переданные уже запущенной программе, открываются в лаунчере сразу, а в студии — после подтверждения.
- **Масштаб интерфейса** — настройка «Масштаб интерфейса» 50–200 % в разделе «Общие», для студии и лаунчера.
- **Переназначаемые горячие клавиши везде** — каждое сочетание клавиш, модификаторов, колеса и кнопок мыши во вкладках перевода, клина, тайпа и на холстах стало именованным действием с любым числом альтернатив, последовательностями клавиш и правилами модификаторов. Настройки › Горячие клавиши переписаны: группировка по областям, добавление/переназначение/удаление/сброс, захват клавиш, кнопок мыши и последовательностей, список конфликтов, поиск. Подсказки на экране всегда показывают текущие назначения. Старые настройки переносятся автоматически один раз.
- **Настройка «Дополнительные окна»** — Автоматически / Отдельные окна / Внутри главного окна.

#### Лаунчер и установщик

- **Предупреждения в настройках** — настройки лаунчера показывают проблемы ИИ: бэкенд не запускается (нет Python, устаревшие файлы бэкенда), сохранённое устройство не найдено, сборка ONNX недоступна или откатывается на CPU, прошлая загрузка ONNX упала, нет папки проектов.
- **Вкладка «Регистрация в системе»** — показывает и чинит записи программы в ОС (Windows: «Удаление программ», App Paths, «Открыть с помощью» и ярлыки; Linux: desktop-файл), с подсказками о правах администратора и подтверждением перед изменением записей другой копии.
- **Замена установленной копии / автономный запуск (Windows)** — если установлена другая копия, окно установки предлагает заменить её этой (меняется только исполняемый файл, окружение Python остаётся) или запустить эту копию автономно.
- **Загрузчик сторонних ИИ-моделей** — файлы Baberu и PaddleOCR-VL скачиваются из закреплённых источников, проверяются по размеру и SHA-256, докачиваются и отменяются.
- **Запуск из исходников** — скрипты `run-dev` для Linux, Windows и macOS обновляют исходники из git, ставят Rust и запускают программу.
- **Руководство для новичков** — новая страница вики с 38 скриншотами.

#### Сенсорный ввод и Android (экспериментально, проверка на устройствах впереди)

- **Жесты пальцами на холстах** — один палец двигает, два — двигают и масштабируют, долгое нажатие открывает контекстное меню или начинает выделение; привязка к оси и инерция. Работает на холстах страниц, в PS-редакторе и в рамках инструментов; в тайпе тап выбирает текстовый слой, а долгое нажатие открывает его меню. Мышь и перо работают с инструментами как раньше. Настройка «Общие» › «Жесты сенсорного экрана» (по умолчанию включена на Android, скрыта на Windows).
- **Порт на Android** — сборка APK (arm64-v8a, Android 12+) с системным выбором файлов, учётом системных отступов, аппаратной клавиатурой, встроенным ONNX-рантаймом (CPU и XNNPACK, ничего не скачивается), хранением ключей AI API и токена Hugging Face в Android Keystore и рамкой OCR, рисуемой пальцем. ИИ-бэкенд и захват экрана на Android недоступны.

### Улучшения

- **Одно главное окно** — лаунчер и студия стали экранами одного окна; при открытии главы показывается экран загрузки, при закрытии — экран сохранения, вся тяжёлая работа идёт вне GUI-потока. «Новый проект», импорт PSD, вики и пакетная обработка могут открываться встроенными окнами.
- **SQLite по умолчанию** — документы хранятся в SQLite (`.db`) в режиме «prod» по умолчанию; режим «dev» оставляет JSON, переключение выполняет глобальная конвертация.
- **Единый менеджер сохранения** — один менеджер пишет черновик сессии раз в 3 минуты или после 30 действий, записей на диск стало гораздо меньше.
- **OCR и перевод через AI API** — свои OpenAI- и Anthropic-совместимые серверы (базовый URL, ключ необязателен); ключ проверяется сразу при вводе, показываются баланс и лимиты; поддержка изображений помечается как «да / нет / неизвестно»; живой статус «Думает… / Отвечает…» со счётчиком символов и кнопкой «Стоп»; терпимый разбор ответов модели (JSON извлекается из текста, мелкие повреждения чинятся, частичные результаты сохраняются, повторно запрашиваются только недостающие ID); подозрительные переводы помечаются предупреждением, а не считаются ошибкой; необязательный «Строгий формат JSON».
- **PaddleOCR-VL** — выбор PaddleOCR-VL 1.6, 1.5 и варианта для манги.
- **Детекция текста на больших страницах** — масштаб и нарезка на тайлы планируются для каждого движка (CTD, PaddleOCR, Surya), план показывается в панели; слишком большие страницы отклоняются с сообщением.
- **Устойчивые загрузки** — ИИ-модели, ONNX-рантайм и установщик используют один движок загрузки: докачка, повтор с паузами (виден в интерфейсе), отмена, проверка размера и SHA-256. Быструю загрузку в лаунчере можно отменить.
- **Диалоги выбора файлов больше не блокируют окно**; системные запросы «да/нет» заменены внутренними диалогами.
- **Быстрее загрузка моделей на AMD (ROCm)** — веса проходят через анонимную память перед копированием на GPU, без зависания драйвера.
- **Недоступный локальный ИИ** — с `--no-ai` или на платформах без локального ИИ панели OCR и перевода остаются на месте и показывают причину; облачный AI API и машинный перевод без ИИ при `--no-ai` продолжают работать.
- **Быстрая загрузка: 19 новых сайтов** — Dandadan, Dankefuerslesen, Dynasty Scans, Hentai2Read, Hiperdex, Kaliscan, Komikcast, Kuaikan, Mangafreak, Manganelo, Mangapark, Mangaread, Mangataro, Naver, Rawkuma, Senmanga, TCB Scans, WeebCentral, Weebdex.
- **Импорт PSD** — файлы PSB; обычные изображения из того же выбора, папки или архива импортируются как страницы; слой можно наложить на клин страницы; превью на шахматном фоне прозрачности.
- **Эффекты тайпа** — мягкий вариант свечения (радиус размытия, смещение, излом, расширение по сторонам); режим области и допуск цвета у градиента; принудительный жирный можно задавать инлайн-тегом и использовать для утончения глифов.
- **Панель тайпа** — висячая пунктуация стала ползунком силы 0–100 %; ввод цвета в hex (#rrggbb / #rrggbbaa); поле ввода на холсте рисует текст в размере рендера; настройки помощи центрирования сохраняются в слое; у глобальных пресетов отдельные создание/переименование/удаление и предупреждение о несохранённых изменениях; переделаны продвинутые формы текста; группы шрифтов показывают имена или названия и выводят состав таблицей; встроенный набор UI-шрифтов доступен как шрифт тайпа.
- **PS-редактор** — лассо и кисть как в Photoshop; рисование по слою клина и скрытие базовых слоёв; переключатели пиксельной сетки и сглаживания; док-панели.
- **Менеджер страниц** — карточки клина («клин ✓» / «Проблема», двухшаговое открепление, раздел «клин без страницы» с привязкой и удалением) и окно просмотра больших изображений в полном разрешении.
- **Вкладка клина** — строка статуса предупреждает, если клин страницы не загружен из-за несовпадения размера и если клин не принадлежит ни одной странице.
- **Новый проект** — ширину ленты можно тянуть, опция «Показывать одинаковой ширины»; холст пакетной обработки получил неподвижную сетку, узлы, не зависящие от масштаба, и неблокирующие диалоги файлов.
- **Перевод** — автодополнение имён персонажей учитывает имена из пузырей главы и служебные имена.
- **Естественная сортировка файлов** — без учёта регистра и с поддержкой кириллицы (`p2` раньше `p10`).
- **Вики** — нумерованные списки, цитаты и ряды изображений отображаются правильно.
- **Совместимость с бэкендом** — предупреждения «версии не совпадают» убраны; совместимость определяется только версией протокола.

### Изменения поведения

- **Новая нумерация версий** — версии теперь обозначаются простым номером сборки (например, 330) вместо 3.x.y; релизы выходят регулярно, и в каждом перечислены только его собственные изменения. Копии версии 3.6.0 обновятся на новую нумерацию автоматически.
- Горячие клавиши панелей перевода требуют точных модификаторов (Shift+S больше не открывает панель OCR).
- По умолчанию пузыри и ручки двигает только левая кнопка мыши.
- Ctrl+колесо над док-панелями больше не масштабирует холст; Ctrl+= масштабирует вокруг курсора.
- Шрифт интерфейса всегда берётся из глобального набора; папка `fonts/ui` тайтла его больше не переопределяет.
- Глиф многоточия по умолчанию удаляется.
- Новые настройки ленты по умолчанию: боковые пузыри сжимаются для манги, статус перевода показывается метками, разрешены две колонки боковых пузырей.

### Исправления

- **Изменения клина доходят до тайпа** — после Ctrl+Z, быстрой чистки или правок в PS-редакторе холст тайпа мог показывать устаревший клин.
- **Экспорт тайпа** использует последние правки PS-редактора, а не устаревшую проекцию.
- **Несохранённые изменения больше не теряются** — отложенные изменения слоёв PS-редактора (видимость, непрозрачность, сворачивание) переживают закрытие студии; правки пузырей во время слияния с проектом оставляют сессию несохранённой.
- **Нет зависаний GUI при записи настроек и словарей** — эти записи теперь идут в фоне.
- **Свечение и обводка** сохраняют прозрачность текста; исчезла тонкая светлая линия между свечением и обводкой.
- **Штамп** — контуры текста больше не просвечивают сквозь заплатку; показывается, откуда берутся пиксели.
- **Вёрстка текста** — перенос слов в верхнем регистре и тег неразрывности в продвинутой форме; горизонтальное выравнивание вдоль кастомных линий и фигур; минимальное расстояние до символа в кастомной раскладке; высота символов в вертикальном тексте; помощь центрирования игнорирует пунктуацию.
- **Ввод текста и слои** — поле создания больше не сдвигает шрифт после выделения; перемещение слоя стрелками обновляет его маску; слои больше не мигают при удалении одного из них; старые текстовые слои 2.x читаются правильно.
- **Холст** — ограничение горизонтальной прокрутки; лента больше не уезжает вправо после перезагрузки и не сдвигается при создании первого или удалении последнего пузыря.
- **OCR через AI API** учитывает «объединение строк»; ошибки чтения заметок, персонажей и терминов показываются, а не игнорируются; проверка маршрута OCR через бэкенд больше не отказывает ложно при первой проверке.
- **Загрузки** — быстрая загрузка с WEBTOONS и MangaDex; порядок страниц, смешанные страницы `<img>`/`<canvas>` и дубликаты при глубоком перехвате (CloakBrowser); расширенный загрузчик снова открывается после ручного закрытия его браузера.
- **Менеджер страниц** — увеличен отступ между карточкой клина и следующим рядом страниц.

### Техническое

- Приложение разделено на 44 крейта `ms-*`; egui/eframe обновлены до 0.36.2.
- Версия протокола бэкенда 16; Python-бэкенд реорганизован по доменным пакетам.
- Страница экспорта в лаунчере предупреждает, что экспорт главы идёт в собственном формате программы.
- Обновление и установка проверяют каждый скачанный файл по контрольным суммам SHA-256 релиза перед использованием.

## Не выпущено

### Новые возможности
- **«Заполнить обведённое» в редакторе области** — кнопка рядом с «Сгенерировать маску» заливает всё, что полностью окружено выбранным слоем маски: обведите область кистью и залейте её одним нажатием. Край области не считается границей.
- **Маска и текстовые блоки из выделения для распознавания** — у PaddleOCR и Surya в панели распознавания появился параметр «Создавать маску и текстовые блоки»: если он включён, выделение области для распознавания заодно ищет в ней текст тем же движком и ставит туда маску и текстовые блоки детектора текста, заменяя старые только внутри выделения. Пока параметр включён, маска и текстовые блоки видны на странице так же, как при открытой панели детектора текста.
- **Распознавание и детекция текста через NeuroLate** — облачный сервис NeuroLate стал новым движком в панели распознавания текста и новым режимом детектора текста во вкладке перевода. API-ключ NeuroLate вводится один раз (он хранится в системном хранилище учётных данных), рядом видно, сколько осталось от его лимита. Язык оригинала (любой из 58 языков NeuroLate, с поиском по списку) и тип комикса (вебтун или манга) можно выбрать или оставить на «Авто». С включённым «Создавать маску и текстовые блоки» один запрос распознавания сразу возвращает и маску текста, и текстовые блоки выделения, без доплаты. В детекторе текста флажок «Сразу распознать текст» тем же запросом читает текст каждого найденного блока и создаёт для каждого блока бабл с этим текстом, как «Распознать блоки» (текст каждого блока тоже оплачивается). NeuroLate платный: каждый запрос тратит токены с баланса NeuroLate, а неудавшийся запрос никогда не отправляется повторно автоматически.
- **PyTorch для AMD Radeon на Windows и Linux** — установщик и раздел PyTorch в лаунчере теперь предлагают GPU-сборку PyTorch для видеокарт AMD (ROCm) и на Windows, а не только на Linux. Архитектура видеокарты определяется автоматически, её можно выбрать вручную; до установки видны размер загрузки и требуемая версия драйвера. Отдельно ставить ROCm или HIP SDK не нужно. На Linux ROCm-сборка PyTorch теперь предлагается на любой машине с поддерживаемой видеокартой, даже без установленного в системе ROCm.
- **Сборка PyTorch в настройках ИИ-бэкенда** — в диагностике CUDA/ROCm видно, какая сборка PyTorch установлена и какую видеокарту она видит; если в установленном PyTorch нет ядер для вашей видеокарты, появляется предупреждение.
- **Машинный перевод через ваш браузер** — Google, DeepL и Яндекс можно по отдельности переключить в режим «Через мой браузер» в панели машинного перевода: тогда переводит расширение ManhwaStudio на обычном сайте сервиса в вашем Chrome, Edge, Яндекс Браузере или Brave — это помогает, когда сервис ограничивает или блокирует запросы программы. Панель устанавливает и обновляет расширение в один клик и показывает три шага, чтобы загрузить его в браузер (открыть страницу расширений, включить режим разработчика, «Загрузить распакованное расширение» из этой папки), а также подключён ли браузер и какой. Если сайт покажет проверку, панель попросит пройти её на его вкладке, и перевод продолжится; «Стоп» по-прежнему работает. С расширением одновременно работает только одно запущенное окно ManhwaStudio — другое окно сообщит об этом. По умолчанию остаются «Встроенные запросы».
- **Пресеты компоновки во вкладке перевода** — в панели «Компоновка» на вкладке «Параметры» появился выбор пресета: текущие параметры (включая сортировку и шаблон MiniJinja) можно сохранить как именованный пресет, переименовать, обновить и удалить. Пресеты общие для всех тайтлов, а выбранный пресет запоминается для каждого тайтла; если параметры изменены относительно пресета, это видно сразу. Есть два встроенных пресета: «Экспорт перевода» и «Перевод через ИИ»; их можно изменить и вернуть к заводским значениям, а названия следуют языку интерфейса.
- **Настройка «Тип тайтла»** — в Настройках › «Лента и пузыри» появилась строка «Тип тайтла» (Страницы / Вебтун), отдельная от пресета раскладки. По ней работают автоматический тип комикса в распознавании NeuroLate и перенарезка по умолчанию при экспорте в PDF; изменение действует сразу, без повторного открытия тайтла.
- **Компактные веса FLUX.2 klein (GGUF)** — в ИИ-редакторе области можно отдельно выбрать Q8_0 / Q6_K / Q5_K_M / Q4_K_M для трансформера и для текстового энкодера, у klein 4B и 9B (у расцензуренного энкодера 9B — Q8_0 / Q6_K / Q4_K_M): у 4B от ~2,6 ГБ + 2,5 ГБ вместо 7,8 ГБ + 8 ГБ на диске и в памяти, ценой более медленного шага генерации. Панель памяти и скорости показывает оба выбора. GGUF-файлы, выбранные вручную из другой папки, тоже работают, если обычная модель 4B установлена.

### Улучшения
- **Остановка детектора текста** — запущенную детекцию текста по нескольким страницам теперь можно остановить: обрабатываемая страница доделывается и применяется, после чего запуск прекращается.
- **Постобработка результатов FLUX klein** — в редакторе области растушёвка края и подгонка цветов к оригиналу вынесены в отдельный раздел «Постобработка» и применяются к показанному результату сразу, без повторной генерации. Цветовые пометки больше не просачиваются в край вставки. Цвета теперь подгоняются и тогда, когда ничего не закрашено (перерисовывается вся область): опорой служат края области.
- **«Скрыть рамку» в редакторе области** — переключатель рядом с «Сравнить» прячет рамку, её ручки, маски и строку состояния, пока результат ждёт применения, чтобы посмотреть, как края области стыкуются со страницей.
- **Сведения о системе в лаунчере** — вкладка «Информация о системе» в настройках лаунчера переработана и работает на Windows, Linux, macOS и Android: система (ОС, процессор, ядра, объём и свободная оперативная память), каждая видеокарта с типом, видеопамятью, архитектурой и драйвером и то, чем ИИ может пользоваться на этой машине, — встроенное ускорение ИИ, сборка PyTorch, которую предложит установщик, и PyTorch, на котором работает ИИ-бэкенд. Список архитектур ROCm 7.2 убран. «Скопировать отчёт» кладёт всю страницу в буфер обмена. Проверка больше не запускает PowerShell на Windows и не зависает, если системная утилита перестала отвечать.
- **Инструменты перевода в закрепляемых панелях** — панель «Инструменты» вкладки перевода и каждый её инструмент (Пузыри, Распознавание текста, Компоновка, Машинный/ИИ перевод, Массовый детектор текста) теперь закрепляемые панели, как остальные панели вкладки. Можно открыть несколько инструментов сразу: кнопки и горячие клавиши включают и выключают каждый инструмент отдельно, а кнопки показывают, какие из них включены. По умолчанию инструменты открываются вкладками одной панели над «Инструментами»; их можно перетащить куда угодно, и они вернутся туда, где вы их оставили. Открытые инструменты запоминаются между запусками. Окно инструмента больше не висит поверх панелей.
- **Машинный перевод с учётом лимитов сервисов** — запросы к Google, DeepL и Яндексу теперь идут в темпе, который допускает каждый сервис, даже когда вкладки перевода и клина переводят одновременно. Если сервис просит сбавить темп, перевод сам пережидает: панель показывает, чего и сколько он ждёт, а «Стоп» сразу прерывает ожидание. Если сервис блокирует запросы надолго, запуск останавливается и сообщает, когда повторить; оставшиеся баблы остаются непереведёнными. При временной блокировке Google перевод переключается на запасной адрес и позже возвращается на основной, а очень длинные тексты отправляются в Google по частям. DeepL и Яндекс переводят несколько баблов за один запрос. DeepL соблюдает свой настоящий лимит (несколько запросов сразу, дальше около одного в минуту) и при отказе останавливается с сообщением «повторите примерно через N мин», а не уходит повторами в долгую блокировку; запуски больше не тратят секунды на новое соединение с DeepL. Зависшее соединение с DeepL или Яндексом больше не подвешивает перевод.
- **Вкладки панелей переносятся на новую строку** — если панель слишком узкая, чтобы все названия её вкладок поместились в одну строку, они продолжаются на следующей строке, а не уходят за край панели. Панель больше нельзя сделать уже, чем самое длинное название её вкладки.
- **Редактор легче в простое** — редактор больше не перерисовывается постоянно, когда ничего не происходит: после быстрой прокрутки в клине или тайпе, при открытой панели масок или редакторах текста в тайпе, при закреплённом сообщении в строке состояния, во время загрузки страниц. Большие главы прокручиваются плавнее во вкладках перевода, клина и тайпа, а вынесенные окна панелей других вкладок больше не тормозят главное окно, пока вы работаете во вкладке без панелей. Тайп больше не подвисает на мгновение при первой прокрутке к странице со слоями фотошопа: её слои появляются чуть позже.
- **Пресеты раскладки** — пресет в Настройках › «Лента и пузыри» теперь задаёт всю раскладку страниц и пузырей (раздельные страницы, отступы, ширина, масштаб, сторона, растяжение и второй столбец боковых пузырей, тип пузырей по умолчанию, раскрытие пузыря поверх) и запоминается для каждого тайтла; изменение параметра помечает пресет как несохранённый вместо переключения на «Свой». Свои пресеты можно создавать, переименовывать, сохранять и удалять (они общие для всех тайтлов); встроенные «Страницы» и «Вебтун» можно изменить и вернуть к заводским значениям, а новые тайтлы получают вашу изменённую версию.
- **Расположение имени персонажа в компоновке** — когда на вкладке «Параметры» панели «Компоновка» включено «Использовать имена персонажей», можно выбрать, ставить имя перед репликой или после неё, и задать текст до и после имени (например, «Имя: реплика» вместо «реплика - Имя»). По умолчанию результат прежний.
- **Прогресс загрузки FLUX.2 / FLUX.3 klein** — чтение модели с диска теперь показывает отдельную полосу для каждой части (трансформер, текстовый энкодер, VAE), обновляемую пять раз в секунду, со скоростью чтения; у FLUX.3 klein она появилась впервые, у FLUX.2 klein добавилось чтение текстового энкодера.
- **Прогноз памяти FLUX.2 klein учитывает текстовый энкодер** — цифры ОЗУ и видеопамяти включают энкодер, если он нужен прогону или уже загружен; он не учитывается, только когда все промпты в кэше и энкодер не загружен.

### Изменения поведения
- **FLUX.2 klein 4B и 9B скачивает само приложение** — закреплённые файлы с контрольными суммами; уже скачанная модель один раз проверяется и остаётся, повторной загрузки нет (трансформер 9B, сохранённый папкой diffusers, тоже остаётся). Для закрытых репозиториев 9B панель прямо говорит, чего не хватает — токена Hugging Face, действующего токена или принятых условий на странице модели — и даёт ссылку на страницу.

### Исправления
- **Сохранение маски детектора текста** — «Сохранить выделение» в панели детектора текста во вкладке перевода теперь сохраняет маску ровно такой, какой вы её видите, вместе с тем, что нарисовано или стёрто кистью маски; раньше эти правки терялись.
- **Определение видеокарт AMD** — машины с видеокартой NVIDIA или Intel больше не принимаются за машины с AMD, а версия ROCm на Linux больше не читается неверно (могла показываться «1.18»).
- **Машинный перевод через Google** — перевод баблов через Google снова работает; раньше каждый бабл завершался ошибкой «Invalid request». Если Google всё же отклоняет запрос, ошибка теперь объясняет причину (временная блокировка, слишком много запросов, проблема с сетью) и что делать.
- **Крепление широкой панели к узкой** — панель, прикреплённую снизу, сверху или сбоку к более короткой соседке, теперь можно поставить в любом месте вдоль неё, лишь бы одна грань оставалась внутри другой; раньше более широкая панель всегда прижималась к левому (или верхнему) краю узкой. Уже настроенные расположения панелей не сдвигаются.
- **Фоновая работа на скрытых вкладках** — машинный перевод, распознавание и детекция текста, запущенные во вкладке перевода, загрузка маски текста и быстрый клин во вкладке клина и отрисовка текста в тайпе теперь продолжаются и доходят до других вкладок, пока вы находитесь на другой вкладке; раньше их результаты ждали, пока вы вернётесь на вкладку, где их запустили.
- **Раскладка тайтла больше не зависит от других тайтлов** — ширина, сжатие, сторона, второй столбец и растяжение боковых пузырей теперь сохраняются в каждом тайтле, а не общие для всех, поэтому настройка одного тайтла больше не меняет раскладку другого. Существующие тайтлы сохраняют свою раскладку, только сжатие боковых пузырей страничного тайтла или вебтуна возвращается к значению его типа. Тип тайтла больше не превращается незаметно в «Свой» при изменении настройки — из-за этого NeuroLate принимал мангу за вебтун, а экспорт в PDF перенарезал страничный тайтл. Выбор «Страницы» / «Вебтун» при первом открытии тайтла теперь задаёт всю его раскладку, а открытые изображения всегда получают пресет «Страницы».
- **Переключатели панели холста не сбрасываются настройками** — «Показывать пузыри» и прозрачность пузырей, изменённые на панели холста, больше не возвращаются назад, когда после этого меняется параметр в Настройках › «Лента и пузыри».
- **FLUX.1-Fill работает без интернета** — после загрузки FLUX.1-Fill инпейнт им больше не обращается к Hugging Face при каждом запуске, поэтому работает офлайн, и сбой сети больше не приводит к ошибке.
- **Отмена загрузки модели срабатывает сразу** — отмена загрузки, которую выполняет AI-бэкенд, теперь останавливает её примерно за секунду даже при зависшем соединении; раньше это могло занимать до минуты. Загрузка, прерванная сбоем или принудительным закрытием, продолжается с уже скачанных данных, а не начинается заново.
- **Восстановление ИИ-окружения** — установщик и проверка окружения теперь приводят существующее ИИ-окружение к проверенным версиям пакетов: доустанавливают пакеты, нужные FLUX.2/FLUX.3 klein и FLUX Fill, держат библиотеку языковых моделей на проверенной версии и исправляют пакеты, установленные не той версии.
- **Детектор текста** — сохранение больше не удаляет и не перезаписывает нечитаемые сохранённые детекции страниц и сообщает, сколько их оставлено; правка строк детектора теперь доходит до быстрой очистки во вкладке клина.
- **Статус OCR** — заменённое или потерянное распознавание больше не оставляет статус «детекция» висеть.
- **Настройки редактирования области** — параметры SDXL, Lama и облачного редактирования сохраняются один раз после остановки регулятора, а не на каждом шаге, записываются безопасно, и последнее изменение больше не теряется при закрытии программы сразу после него. Если проверки статуса облачного задания окончательно не удались, задание отменяется и у fal, Replicate или Runway. Отмена без готового результата пишет «Обработка отменена.», а не «результат отброшен».
- **Быстрая очистка текста** — маски текста, сохранённые детектором в уменьшенном размере, теперь правильно относятся к своей странице.
- **Пресеты формул** — ошибка сохранения теперь показывается в строке состояния панели, а не теряется молча, два быстрых сохранения больше не могут записаться в обратном порядке, а имена пресетов сохраняются ровно так, как введены.
- **Статус сохранения и сброса** — строка статуса обновляется сразу по завершении сохранения в проект или сброса, без движения мыши.
- **Доступность быстрого OCR** — быстрое распознавание на холсте перевода учитывает текущее состояние ИИ-бэкенда, даже когда все инструменты скрыты.
- **Сенсорный ввод** — касание, отменённое системой (например, при отбрасывании ладони), больше не оставляет указатель зажатым.
- **Сообщение об ошибке SDXL** — если библиотека SDXL не загружается, ошибка показывает настоящую причину, а не всегда просит её установить.
- **Интервалы в тексте справа налево** — иврит и другой текст справа налево больше не идёт задом наперёд и не сбивается по интервалам при кернинге «Метрический» и оптическом; собственные кернинговые пары сдвигают буквы такого текста ровно на заданную величину (значение 0 больше не смещает буквы); искусственное полужирное начертание расширяет такой текст, а не сжимает его.
- **Строки справа налево стоят на месте** — текст на иврите и других языках справа налево, длина которого меняется из-за пользовательских кернинговых пар, кернинга «Метрический» или оптического либо искусственного полужирного, теперь выравнивается и переносится по своей настоящей ширине: у сжатой строки нет пустоты слева, а расширенная не вылезает за левый край; перенос строк также учитывает искусственный полужирный.
- **Межбуквенный интервал в тексте справа налево** — положительный интервал теперь раздвигает буквы иврита и другого текста справа налево (а отрицательный сжимает), а не наоборот.
- **Кисть и штамп в клине** — слой клина больше не мигает в конце мазка кистью или штампом, а остальные страницы на экране не теряют клин во время рисования.
- **Пользовательские кернинговые пары и переносы строк** — перенос строк и подбор форм текста теперь учитывают ваши кернинговые пары ровно так, как они рисуются: строка, которая помещается благодаря сжатым парам, больше не переносится раньше времени, а пары, нарисованные запасным шрифтом или слитые в лигатуру, больше не искажают замер.
- **Переносы строк учитывают межбуквенный интервал** — текст с ручным интервалом между буквами (в px или %) или с кернингом «Метрический» теперь переносится там, где реально кончаются нарисованные строки, а не вылезает за рамку текста; окно форм текста подбирает варианты так же.
- **Восстановление окружения Python** — в окружениях от старых версий больше не остаются два конфликтующих пакета OpenCV: лишний удаляется, нужный переустанавливается; сообщение проверки теперь говорит «отсутствуют или устарели пакеты».
- **Бабблы-картинки** — вставка картинки из буфера обмена или выбор файла картинки для баббла больше не подвешивает окно на время сохранения, а прерванное сохранение не оставляет повреждённый файл.
- **Окно «Новый проект»** — выпадающие списки и ползунок потоков загрузки реагируют на колесо мыши, как во всём приложении, не прокручивая панель.
- **Расцензуренный текстовый энкодер FLUX.2 klein 9B** — теперь кодирует промпты с правильными настройками позиций (раньше они неверно читались из его конфигурации); закэшированные с ним промпты один раз кодируются заново.
