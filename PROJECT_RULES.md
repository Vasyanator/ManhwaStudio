# PROJECT_RULES — ManhwaStudio

Project layer of the user's global agent instructions (`~/.claude/CLAUDE.md` + `~/.claude/rules/*.md`;
"global §N" points there): it only fills in and specializes those rules for this repository and
never restates them. It overrides exactly two global rules, each marked **Overrides** below:
the clippy/test commands of `rules/rust.md` "Toolchain and verification" (workspace-wide here) and
the file-size calibration of global §2.5 (recalibrated counts). Architecture lives in
`ARCHITECTURE.md`, module detail in each directory's `MODULE_README.md`; both root files reach
agent context through the gitignored `CLAUDE.local.md` that the SessionStart hook maintains.

---

## Toolchain and targets (global §10, §16)

- Rust edition 2024. The MSRV lives ONLY in the root `Cargo.toml` `rust-version` (the run-dev
  launchers parse it); never copy the number elsewhere.
- Mandatory targets for every change: `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`.
  The windows-gnu check needs `x86_64-w64-mingw32-gcc` (bundled SQLite in `ms-docstore`).
- The web build (`wasm32-unknown-unknown`, `cargo +nightly wcheck`, `dev-docs/WEB_PORT.md`) is
  INACTIVE: it is not a check target and a change need not keep it building (the user's decision;
  the open blocker is `STATE.md` S-004). Keep existing wasm gating and `ms_thread` use intact; do
  not delete web code.
- macOS (`x86_64-apple-darwin`) is supported but NOT a mandatory check target: `cargo check-mac`
  / `cargo bmac` (zigbuild; a plain `cargo check --target x86_64-apple-darwin` fails in ring's C
  build, per crate use `cargo zigbuild --target x86_64-apple-darwin -p <crate>`), `run-dev.MacOS.command`, `build-macos.sh`.
- node/npm are optional: with them, `ms-browser-ext`'s build script builds `browser-extension/`
  (TypeScript) into the embedded bundle; without them, or with `MS_SKIP_BROWSER_EXTENSION=1`, the
  build succeeds without the extension (a `cargo:warning`, the UI says "not included"). There is
  no strict mode that fails a build on a missing extension.
- For Windows-target builds (including `cargo check-all`) set `MS_DISABLE_BUILD_CODESIGN=1`, or
  `build.rs` re-spawns the codesign worker on every source change. Under `flock` that detached
  worker inherits the lock fd and holds the lock for up to ~25 min (it waits for `.exe` files a
  `check` never produces), stalling every queued cargo call.

## Verification commands

- `cargo check-all` — alias in `.cargo/config.toml`: quiet `cargo check` for both mandatory
  targets. It has no `--all-targets`, so it never sees test code.
- **Overrides `rules/rust.md` "Toolchain and verification":** the workspace's default member is
  the root package only, so a bare `cargo clippy --all-targets` lints `src/` and NOTHING in
  `crates/`. Use `cargo clippy --workspace --all-targets -- -D warnings`, or
  `cargo clippy -p <crate> --all-targets -- -D warnings` for every touched crate. Tests likewise:
  `cargo test -p <crate>` per touched crate, or `cargo test --workspace`.
- Never run cargo builds of this workspace in parallel (several sub-agents each running
  `cargo test` / `clippy` at once OOM-killed a 60 GB machine and the whole session). The manager
  serializes verification across agents: every cargo invocation of a parallel agent is wrapped
  in one shared `flock <lockfile> cargo … -j 8`, and agents skip Serena (its server costs GBs).
  With two or more such workers, use the deferred verification mode (global §2.1): a worker
  runs only `flock <lockfile> cargo check -p <its crate> -j 8` (plus the same with
  `cargo ndk -t arm64-v8a --platform 31` for Android-port work); the manager then runs
  `cargo check-all`, `cargo clippy --workspace --all-targets -- -D warnings` (linux and
  `--target x86_64-pc-windows-gnu`), `cargo test -p <every touched crate>` and the Android check,
  each with `--message-format short` where cargo supports it.
- Changes under `browser-extension/` (or to the wire protocol): `npm --prefix browser-extension test`.
- Android (`aarch64-linux-android`, not a mandatory target yet): check / lint the root library
  with `cargo ndk -t arm64-v8a --platform 31 clippy -p manhwastudio_rs --lib -- -D warnings`
  (`ANDROID_NDK_HOME` set); the APK is built only by `./build-android.sh` (`--install` deploys;
  see `android/README.md`). Its cdylib comes from `cargo rustc --crate-type cdylib`: never add
  `cdylib` to the root `[lib]`, every desktop build would then link one.
- Cargo unifies features across the workspace, so a crate can build inside it and fail alone.
  After changing a crate's manifest or features, also run
  `cargo clippy -p <crate> --all-targets --all-features --target <t> -- -D warnings` per target.
- **Baseline build of `HEAD`** (global §19 "Unrelated must be proven", when the dependency-graph
  proof is not enough): from a `git worktree` of `HEAD` under the scratchpad, run
  `flock <lockfile> env CARGO_TARGET_DIR=<repo>/target/head-baseline CARGO_BUILD_BUILD_DIR=<shared build-dir>/head-baseline cargo test -p <crate> -j 8`.
  The persistent `target/head-baseline` makes every check after the first incremental. Never
  build the worktree into the main `target/`: its binaries would overwrite
  `target/release/manhwastudio_rs` and mislead the stale-build check below.
- **One shared cargo `build-dir`, one checkout per subdirectory.** The user's `~/.cargo/config.toml`
  sets a single `build-dir` shared by all projects (disk space). Cargo names a workspace crate's
  intermediates by its path RELATIVE to the workspace root, so two checkouts of this repository
  (a `git worktree`, the baseline above) overwrite each other's artefacts there and a later
  `cargo check` trusts the stale ones (seen: "no `Flux2EncoderSource`" from a worktree's older
  ms-config). The main checkout uses the shared dir as is; every other checkout sets
  `CARGO_BUILD_BUILD_DIR=<shared build-dir>/<checkout name>` and that subdirectory is deleted with
  the checkout. After a collision, `cargo clean --workspace` in the main checkout repairs it
  (dependencies stay cached).
- **Two runnable binaries** (global §16 "Stale builds"): `target/release/manhwastudio_rs` and
  the repo-root `./manhwastudio_rs` that the `run-dev.*` launchers publish. Find out which one the
  user ran, then compare its mtime (`stat -c '%y' <binary>`) with
  `git diff --name-only | grep '\.rs$'`.

## Workspace and crate rules

- Third-party versions live only in root `[workspace.dependencies]` (rules in its header
  comment); a crate writes `<dep>.workspace = true`. Internal `ms-*` dependencies stay local
  `path =` edges. Only the vendored `crates/puffin_egui` is outside the table.
- Cargo features are not inherited: a feature must be declared in every crate that gates on it
  and forwarded from the root. `tutorial` is the only forwarded one; `profiling`, `inspection`,
  `active-logs`, `web` are root-only.
- `#[cfg(test)]` items are invisible to dependents: helpers needed by tests further up the stack
  are `#[cfg(any(test, feature = "test-support"))]`, enabled only from `[dev-dependencies]`
  (examples: `ms-models`, `ms-config`).
- Crates extracted from the binary do not enable `clippy::pedantic`; crates written fresh do.
- The `crate::x` re-export shims in `src/lib.rs` and `src/tabs/mod.rs` are root-package-local (`pub(crate)`)
  compatibility aliases for code still in `src/`. Not every crate has one (e.g. `ms-docstore`,
  `ms-fonts`, `ms-i18n`, `ms-text-render` are named `ms_x::` directly); a new crate needs none.
- Thread creation on any code path that also runs on wasm goes through `ms_thread`; bare
  `std::thread` only in `cfg(not(target_arch = "wasm32"))` code.
- Any Rust <-> Python IPC contract change (method, field, topic, meaning, blob format, on-disk
  `user_config` semantics) bumps `PROTOCOL_VERSION` in BOTH
  `crates/ms-backend-ipc/src/protocol.rs` and `modules/ai_backend/ipc/protocol.py`; never judge
  whether it is "breaking".
- One owner each, never duplicate: canvas layout presets — the preset-owned field set, factory
  values, the layout's JSON codec and per-title migration, the app-wide preset store and what a
  built-in means after a user edit (`ms_project::canvas_layout`, `CanvasLayoutPresets::effective_builtin`), custom-font synthesis, recipe key and compiled-face naming
  (`ms-custom-fonts`; `RecipeKey::full_face_post_script_name`), custom-font identities (saved /
  ad-hoc / render names: `ms-tab-typing` `custom_fonts::identity`), polygon rasterizer (`ms_raster::fill_polygon_spans`,
  re-exported as `ms_tools::fill_polygon_spans`; sole named exception: the Paddle glyph mask in
  `ms_text_detect::glyph_mask` keeps imageproc's boundary-inclusive `draw_polygon_mut`, because
  its gates are calibrated on OpenCV `fillPoly` coverage and the spans rule breaks fixture
  parity), square binary dilation
  (`ms_raster::dilate_square`), Otsu threshold (`ms_raster::otsu_threshold`), enclosed-hole fill of a binary mask (`ms_raster::fill_enclosed`), exact Euclidean
  distance transform (`ms_raster::squared_distance_to_mask` / `_with_costs`), region-edit result
  post-processing — colour match, inward feather, composite, the cv2-ellipse mask dilation, the
  post settings' defaults / ranges and their UI body (`ms-region-postprocess`; the Python
  backend returns raw FLUX klein output and never post-processes),
  sRGB <-> CIELAB conversion (`ms_region_postprocess::LabTables` + `encode_srgb`; a caller may own
  only its quantization), the ordered preset writer thread and free default-name allocator
  (`ms_config::preset_store`), the advance between two adjacent glyphs — override guard and
  magnitude, `Fixed` step, run-axis orientation, tracking basis, wrap / form width measurement of
  overrides (`ms_text_render::pair_advance`), atomic replacement of a file's contents and its temp names
  (`ms_atomic_file::write_atomic` / `write_atomic_with`; publishing an already-staged file uses its
  `rename_with_retry` + `sync_directory`; no private temp+rename copy), SOR
  kernel (`ms_tools::red_black_sor_sweeps`), pixel-inspection threshold and grid
  (`ms_canvas::pixel_inspection_recommended_for`, `pixel_grid::draw_pixel_grid`), page <-> clean
  binding (`ms_page_ops::clean_binding` + `ms_models::clean_assign`), text-detection scale and
  tiling decision (`ms_text_detect::plan_detection`, used by the detector worker and panel notice),
  local-AI route availability (`ms_local_ai::local_ai::AiRouteKind::availability`: never test
  `--no-ai` or the platform for an AI route anywhere else),
  backend memory budget (usable memory: `modules/ai_backend/runtime/memory_budget.py`; ledger and
  reclaim: `runtime/residency/`), large-model placement / forecast / degrade
  (`runtime/large_models/`: a model service declares specs and phases, never moves a weight),
  checkpoint -> meta-module filling (`runtime.safetensors_stream.fill_module_from_safetensors`),
  weight-container dispatch (`runtime/tensor_container`; GGUF header parsing `runtime/gguf_stream`),
  GGUF dequantizing layers (`runtime/gguf_layers`; never set `DIFFUSERS_GGUF_CUDA_KERNELS`, ROCm
  reports a CUDA-like capability), the FLUX.2 weight-format list (`ms_config::Flux2WeightFormat`;
  Python lists no quants), llama.cpp <-> HF Qwen3 tensor names (the `gguf` package's own map via
  `flux2_klein/encoder_gguf.py`), adoption of present-but-unmarked catalog files
  (`ms_sysprobe::ai_models::external::adopt_external_model`), the FLUX.2 encoder source and its
  offered formats (`ms_config::Flux2EncoderSource` + `Flux2WeightFormat::offered_for`), the
  Hugging Face gated-access check (`ms_sysprobe::ai_models::hf_access`), the uncensored encoder's
  `rope_parameters` normalization (`flux2_klein/components.text_encoder_config_kwargs`),
  OS integration records — record names (`ms_os_integration::identity`), expected registry
  values (`ms_os_integration::windows::values`) and the "Open with" ownership rule
  (`open_with_command_targets_install`); no other crate spells a record name or builds its values;
  platform capability decisions (`ms_platform::PlatformFeature`: never re-derive a capability from
  `cfg`), file dialogs (`ms_platform::file_dialog`), system clipboard reads
  (`ms_platform::clipboard`), finger-gesture rules on canvases (`ms_input::touch`: no
  canvas or tool reads raw `Event::Touch` or decides pan/pinch/long-press itself), input
  bindings, their display labels and conflicts (`ms-input`: `InputRegistry`, `InputRouter`),
  AMD PCI device id -> gfx (`ms_sysprobe::amd_adapters`: one pinned table; KFD topology wins on
  Linux), the PCI display-device walk (`amd_adapters::enumerate_display_devices`), time-bounded
  probe subprocesses (`ms_sysprobe::gpu_utils::run_command_bounded`), torch pins and ROCm device extras (`ms_installer::torch::catalog`), the backend's
  "which torch build is this" (`modules/ai_backend/runtime/torch_flavour.py`), machine-translation
  request pacing, cooldowns and per-service limits (`ms-tab-translation` `mt_limits`: one table, one
  process-wide registry; a provider takes a permit per HTTP request and never sleeps or throttles
  itself) and MT HTTP timeouts (`machine_translators::http`), the browser-bridge wire protocol
  (`ms_browser_bridge::protocol` + `browser-extension/src/core/protocol.ts`: any message, field or
  code change bumps `PROTOCOL_VERSION` in BOTH; the handshake shapes are frozen), the browser
  extension's id, bundle and unpacked folder (`ms-browser-ext`; the app's bridge config and every
  unpack go through `ms_browser_bridge::app`), opening URLs and folders in other programs
  (`ms_platform::shell_open`).
- ROCm PyTorch comes only from AMD's whl-next index in its OWN uv call (`--index-url` alone):
  PyPI squats AMD's package names, so the two are never resolved together; every later PyPI
  install passes the `-c` torch constraints file (`ms_installer::torch::constraints`).
- Python backend on ROCm: never advise or enable `expandable_segments:True`; checkpoint weights
  move to the GPU only via `modules/ai_backend/runtime/rocm_mmap_transfer.py`; every
  user-facing backend error text passes through `runtime/error_text.py::sanitize_torch_error`.

## Backend memory (`modules/ai_backend/runtime/`)

- Never plan device memory above `total - reserve`; budget on reserved bytes / `mem_get_info`,
  never on allocated bytes (the allocator's cache and the context are real usage).
- Release the allocator cache (`clear_torch_cache`) at every phase boundary: the large-model
  forecast assumes it, and carried-over cache is the main source of under-forecasts.
- Park a host copy as one buffer per block, never per tensor: glibc keeps small frees in RSS.
- Count host memory as `RssAnon + RssShmem`: CUDA pinned memory shows up as `RssShmem`.
- Never rsync/scp the whole `modules/` directory to another machine: it holds private browser
  profiles (`modules/browser_profiles/`). Copy only the files a measurement needs.
- Regenerate a calibration table (`runtime/large_models/calibration/<device_class>.json`) with
  `venv/bin/python tools/large_model_calibration` on an idle GPU: `flux2` / `flux3` measure,
  `fit <reports> --base <table> --out <table>`, then `check` a validation grid (its `MODULE_README.md`).

## Input (`ms-input`)

- The input-migrated code — `ms-canvas`, `ms-tools`, `ms-tab-translation`, `ms-tab-cleaning`,
  `ms-tab-typing` and `src/studio/` — never reads keys or modifiers raw (`key_pressed` /
  `key_down` / `consume_key` / `consume_shortcut` / `modifiers`) and never makes a key, a modifier,
  the wheel or a mouse button into a shortcut or tool gesture by itself: it declares an action in
  `ms-input` and reads the frame's `InputFrame` (widget-local pointer state such as egui's own
  click / drag responses stays as it is; sole exception: the dev-only `profiling` feature's F11
  profiler toggle in `src/studio.rs`). The PS editor, page manager and launcher are not
  migrated yet and still read raw input. Enforced by a per-crate `clippy.toml`
  (`disallowed-methods`: `egui::InputState::{key_pressed, key_down, key_released, consume_key,
  consume_shortcut}`) in each migrated crate; clippy reads it from the crate's own directory only,
  so never add a root `clippy.toml` (it would reach every crate without its own file).
- Hint and label text never spells a binding: it takes a `{placeholder}` filled from
  `InputRegistry::label` (or `label_with_prefix` / `param_group_label`). Guarded for the migrated
  namespaces by `crates/ms-i18n/tests/binding_text.rs`; `input.*` holds the button names.
- Action ids are persistence keys of the user's overrides (`user_config` -> `Hotkeys`): never
  rename one without a migration in `ms_input::persist`.

## Network downloads

Any code that downloads anything (models, runtimes, code, user content; Rust or Python) must
survive an unstable network:
- transient failures (connect/DNS/reset, read timeout, a body shorter than announced, HTTP
  408/429/5xx) are retried automatically with capped exponential backoff, each retry resuming
  from the bytes already staged (HTTP `Range`) when the server supports it; the attempt budget
  resets after a retry that made progress; permanent failures (other 4xx, hash/size mismatch,
  local disk errors) are not retried;
- staged bytes are discarded only when they are proven wrong (hash mismatch, overflow), never
  because a transfer stopped early; an interrupted run resumes on the next start;
- every request has connect and read timeouts; cancel interrupts both the transfer and the
  backoff wait;
- retries are logged with context and visible in the UI as progress, not as a failure.
Reference implementation: `ms_download` (engine) + `ms_sysprobe::ai_models::external` (pinned
multi-file use). Tests use an injected fetcher and
an injectable sleep, never the network.

## House style: no formatter (global §4)

The tree is NOT rustfmt-clean: long signatures, calls and literals deliberately stay on one line.
Never run `cargo fmt`/`rustfmt` over project sources, in any role. If an edit arrives
reformatted, do not read the raw diff; normalize both sides and diff those ($S = your scratchpad):

```bash
git show HEAD:<file> > $S/o.rs && cp <file> $S/n.rs \
  && rustfmt --edition 2024 --quiet $S/o.rs $S/n.rs && diff $S/o.rs $S/n.rs
```

Reformatting is acceptable only once proven inert this way.

## egui: never write it from memory

- The build pins egui/eframe **0.36.2** (`Cargo.lock`, `egui-docs/VERSION`). Recalled egui —
  `eframe::App::update`, `SidePanel`, `TopBottomPanel`, `Context::screen_rect`, `Rounding`,
  container `id_source`, `InputState::raw_scroll_delta` — does not exist here.
- Before touching egui code read `egui-docs/README.md`, then the page for EVERY widget and
  mechanic the change touches, before writing it: panels/windows -> `01-app-shell.md`; painting
  -> `02-painting.md`; clicks, drags, wheel, hotkeys -> `03-input.md`; widgets and settings panes
  -> `04-widgets.md`; localized labels or stored widget state -> `05-ids-and-i18n.md`; anything
  drawn over other UI -> `06-overlays.md`.
- An API exists only if the generated index has it:
  `grep -P '^egui::Panel::top\t' egui-docs/api/symbols.txt`.
- New UI: the most specific existing widget first — the project set in
  `crates/ms-widgets/src/` (listed in `04-widgets.md`), then egui's own (`egui-docs/api/egui.md`);
  justify a new widget. Fixed decisions: `egui::Slider` / `ComboBox` / `DragValue` are forbidden
  in product UI (`Wheel*` replacements; catalogs use `SearchableComboBox`); every floating panel
  is a panel-dock tab (`CollapsiblePanel` + `PanelTab`) — an `Area` + `Frame::popup` or an
  `egui::Window` used as a panel is a defect, dialogs stay `egui::Window`; AI-gated actions use
  `AiButton`; settings panes shared by studio and launcher use the double-interface pattern
  (`04-widgets.md` §7). A new widget has no literal strings, a stable `id_salt` on any localized
  label, and no blocking work on the GUI thread.
- Every product `eframe::run_native` creator calls `ms_widgets::ui_fonts::install*` exactly once
  and applies the UI scale (`apply_ui_scale_to_context`). Never call `Context::set_fonts` (drops
  runtime-registered families -> epaint panic); never call `Context::fonts*` before the first
  frame or off the GUI thread.
- An egui/eframe upgrade is complete only after `tools/egui_docs/build.sh` ran and its diff was
  reviewed (`python3 tools/egui_docs/check_sync.py` fails on drift from `Cargo.lock`). A new egui
  fact goes into the relevant page with a `file:line` citation into the crate source.

## Localization (i18n)

- No user-visible literal in `.rs`: add a semantic English key
  `<area>.<screen_or_module>.<meaning>` with a role suffix (`_label`, `_hint`, `_button`,
  `_title`, `_error`, `_tooltip`, `_status`) and call it via `t!` / `tf!` / `tp!`.
- Every new key goes into `crates/ms-i18n/locales/en.json` (reference and fallback) AND
  `ru.json`. `es` / `fr` / `pt` are translated separately and fall back to `en`; do not block on
  them.
- If a string's meaning or context is unclear, ask the user; never invent a translation.
- Never change the `{placeholder}` set of an existing key: the editable on-disk catalog
  (`locale/*.json`, reconciled by `ms_config::locale_store`, which adds keys and never
  overwrites) keeps the old text on every existing install, so the new arguments are not
  substituted (seen: «Продолжить загрузку ({done} из {total})»). Add a key with a new id instead
  and remove the old one.
- A message that names a button substitutes the button's label via `{button}`, never a literal
  copy.
- Exceptions (logs, protocol ids, persistence keys, on-disk names, probes) only per
  `dev-docs/i18n_exclusions.md`, with a comment. Some Russian literals are persistence keys
  (formula presets, batch-processing node sockets, `General.enabled_tabs`): never translate them.
- Localized `from_label` / `Window::new` / `CollapsingHeader::new` / `ui.collapsing` sites get a
  stable `id_salt` (the i18n key itself).

## Code navigation and live-app automation

- Read `SERENA.md` before any `mcp__serena__*` call (it replaces Serena's own manual). Wasm- and
  windows-gated modules (e.g. `src/lib.rs` `mod web_entry`) are invisible to it — confirm
  reference searches with grep. Use grep for i18n keys, locale JSON, `MODULE_README.md`,
  `dev-docs/` and every `egui-docs/` lookup.
- Live-app automation (global §16 "Manual UI Verification") is the `/egui-mcp` skill (egui
  inspection protocol) — only on the user's explicit request.

## Test hygiene: project instances (global §16 "Test Hygiene")

- Never written by a test: any owned document (`user_config`, `fonts/fonts_data`,
  `fonts/presets`, … as `.json` or `.db` per storage mode), the `locale/` files, a project's
  `_unsaved/` staging directory, the OS credential store.
- Reference suppression: `crates/ms-tab-typing/src/panel/font_settings_store.rs`
  `persistence_suppressed_by_tests` — `cfg!(test)` OR a sticky runtime latch armed by the
  test-only doors `test_lock` / `test_reset`.
- Path resolvers: `ms_config::program_dir()`, `ms_config::data_dir()`,
  `ms_config::storage_mode::app_fonts_dir()`, `std::env::temp_dir()`.
- Platform-fixed-location exception example: the system-font table in
  `crates/ms-widgets/src/ui_fonts.rs`.

## Where facts go

- `dev-docs/` (gitignored, never published): plans, reports, migration notes,
  machine-local notes, `i18n_exclusions.md`. Known gaps and accepted debt (global §2.5 option b):
  the nearest `MODULE_README.md` or `dev-docs/known_gaps.md` (entry shape `KG-NNN`, defined at
  its top).
- `docs/` (tracked, published): user-facing documentation including the `README` translations;
  must be accurate.
- Root `.gitignore` is a publication allowlist (`*`, then `!path`): a new tracked file or asset
  needs a `!` entry.
- `old_or_test/` is archived 2.x code: never an architecture reference.
- Update `ARCHITECTURE.md` only when a layer, a crate dependency edge, a shared model, a
  cross-layer data flow, the `CanvasView` / `CanvasHooks` contract, the Rust <-> Python contract,
  app-managed AI model gating or worker coordination changes. Everything narrower goes to the
  owning `MODULE_README.md`.
- Update `PROJECT_RULES.md` when you learn a project-specific agent rule the hard way (a tool
  pitfall, a verification command, a house-style fact). Never restate a global rule here.

## Changelog and releases

- **`CHANGELOG.md` (tracked) is kept by the agent after every task** whose result a user can
  notice (feature, behaviour change, fix, new setting, removed option). Add the entry in BOTH
  halves: English under the trailing `## Unreleased`, Russian under the trailing
  `## Не выпущено`, inside the matching `###` category (New features / Improvements / Behaviour
  changes / Fixes / Technical; Russian: Новые возможности / Улучшения / Изменения поведения /
  Исправления / Техническое), creating the category if absent. One bullet per feature,
  `- **Name** — what the user gets`, written for users, no file paths or internal names. Extend
  an existing bullet of the same unreleased feature instead of adding a second one. Pure
  refactors with no visible effect get no entry. Never edit a released `## N — date` block.
- Versions are commit counts (`git rev-list --count`); a release is tag `N` + root `Cargo.toml`
  `N.0.0`, made only by the private `release.py` (its docstring is the manual), which turns the
  unreleased blocks into `## N — date` and publishes them as the GitHub release notes. Never edit
  the version, create a numeric tag or a GitHub release by hand.
- Frozen by the installed 3.6.0 updater (it takes the FIRST release in GitHub's list carrying
  `ManhwaStudio.zip` + its platform binary, compares `tag_name` with its own `--version`): the
  asset names, the hidden `--continue-update` flag, `--version` printing the version as its last
  whitespace token, plain numeric tags, and never a pre-release carrying those assets.

## File-size calibration (global §2.5)

**Overrides global §2.5 "File size" calibration** (thresholds unchanged, counts are this
repository's). Measured 2026-10-25 over `src/` and `crates/` (excluding `target/` and the external
`egui-large-image`, `egui-shader-layers`, `ag-psd` checkouts and the gitignored `egui` / `winit`
forks), after every file over 5000 lines was split into a directory module: 27 Rust files exceed 3000
lines and 2 exceed 5000; without the exempt test modules, 23 and 0. The largest non-test files are
`crates/ms-models/src/layer_model/persist.rs` (~4960), `crates/ms-tab-cleaning/src/tools/watermark_entry.rs`
(~4660) and `crates/ms-installer/src/utils.rs` (~4510) — the next feature that adds a responsibility to
one of them should split it first. Scale example: custom kerning took
`src/tabs/settings/typesetting/font_properties_window.rs` from 1230 to 2467 lines in one change —
nothing should have stopped that one, but the NEXT feature to land there must pause.
`src/studio.rs` (the editor root, ~1500 lines, under both thresholds) is a hotspot: sequence
parallel tasks that touch it.
## Canonical "one decision, many owners" case (global §2.5)

The advance between two adjacent glyphs used to be decided in FIVE places —
`pipeline::run_layout::horizontal_run_layout`, `pipeline::run_layout::optical_horizontal_run_layout`,
`formula::render::seeds::assign_formula_seed_advances`, `wrap::forms::GlyphWidths::build` and
`wrap::horizontal::WrapScoringContext` (all in `crates/ms-text-render/src/`). User-authored kerning
pairs therefore had to be implemented five times; a right-to-left direction bug was fixed in two of them
and reasoned about separately in a third, and the right-to-left anchor itself was wrong everywhere. The two
width measurements could only approximate the pen's same-face guard (by character coverage), and the
capacity estimate ignored overrides entirely, so an override never moved a line break. The decision now
has one owner, `ms_text_render::pair_advance` (override guard and magnitude, `Fixed` step, run-axis
orientation and the right-to-left anchor, tracking basis); the five sites only accumulate its answer, the
wrap measures over the shaped glyphs with the pen's own guard, and recorded bit-exact matrices
(`tests/fixtures/pair_advance_*.json`) pin all of it. The vertical path still applies no pair kerning (an
open design question, not a copy). One owner from the start would have cost one implementation and no
approximation.

