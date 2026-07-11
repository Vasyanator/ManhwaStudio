# models/layer_model

Unified layer model shared between the PS editor and (in later phases) the typing tab. The goal is
one notion of "layer" for both tabs: a normal raster layer and a text layer are the same node,
differing only in metadata and in which operations they permit.

## Model (design)
- **Normal (raster)** — pixels from any source (pasted, cut out of another layer, or rasterized
  text). Can be painted, deformed, cut, merged, transformed, and run through the effects render.
- **Text** — re-renderable from its text params (render type 1) and editable as text. Can be
  deformed, transformed, and run through the effects render. Cannot be painted / cut / merged
  without first rasterizing (a later text render would discard those edits).
- **Group** — a folder of layers. A PS-editor group (`GroupRec`, referenced by `LayerRec.group_uid`)
  may now mix raster AND text nodes; it is orthogonal to a text node's `layer_idx` (the typing tab's
  «Группа текста N» axis). See `tabs/ps_editor/tree.rs` for the unified panel tree that consumes it.

Two render types: (1) **text render** regenerates a text layer's base image from its parameters;
(2) **effects render** applies a post-effects chain over a *preserved* base image and works on any
non-group layer — so every effected layer stores its pre-effects base separately. Rasterizing a
text layer freezes its current render into a normal raster base, drops the text params, and keeps
the effects chain and deform.

Per-kind capability gating (paint / clip / merge / text-render / …) is done inline by the tabs against
`LayerKind`; the old design-only `LayerKindRec::can_*` table was removed in Phase D (never wired).

## Persistence
On disk under `{chapter}/layers/` (staged in `{chapter}_unsaved/layers/`, merged on "save to
project"):
- `layers.json` — `LayersManifest`: explicit `schema_version` (now **3**). v2 added
  `LayerRec.pinned_by_group` + `GroupRec.collapsed`; **v3 inlines the TEXT payload** onto a text
  `LayerRec` (`render_data`, the rendered-PNG name in `rendered_file`, `mask_clip`,
  `overlay_is_image`, and the reused
  `transform`/`deform` geometry), so `layers.json` is self-sufficient for text and `text_info.json`
  is read-only legacy. All v3 fields are serde-default `Option`s, so a v2 file still reads cleanly.
  Per-page trees of `LayerRec` nodes ordered bottom-to-top by `z`; carries `groups`, `deform`,
  `effects`, `payload_ref` into `text_images/`; `group_uid` is populated on both raster and text nodes.
- `ps_p{page:04}_{uid}.png` — each raster layer's pre-effects base pixels.
- `ps_p{page:04}_{uid}_text.png` — a TEXT node's rendered image (the displayed text render). Written
  by the doc text flush only when the in-memory render is dirty or the file is missing (an unchanged
  text PNG is never re-encoded, mirroring the raster `pixels_dirty` rule).

### Text geometry encoding
A TEXT node reuses the canonical `transform`/`deform` fields (rotation in **radians**, same as a
raster), NOT the legacy `text_info.json` degree encoding. The inline v3 payload needs no conversion.
The PS-owned `layer_idx` (text-group axis), carried on the doc node, plus pin/z/group fields are
preserved across a doc flush.

### `text_payload` is the SINGLE text-geometry codec (read + write)
All overlay-geometry decode/encode for BOTH tabs and the doc lives in `text_payload`, so an old chapter
resolves identically everywhere (a critical fix: the doc, now the authoritative text writer, would
otherwise snap legacy text to page-center and bake that into the inline payload, losing the original
geometry permanently).
- READ — `decode_overlay_placement(obj, page_size)` covers the full legacy per-entry vocabulary:
  position `img_x_px`/`img_y_px` (page px) → `img_u`/`img_v` or bare `u`/`v` (center-anchor normalized,
  page-relative) → page center; rotation `rotation_deg` or its `angle` alias (deg→rad); scale `scale`
  or its `user_scale` alias; deform `deform_mesh` (`points_px` or legacy `points_uv`) or a `transform_uv`
  quad expanded to a 13×13 projective mesh. `decode_deform_mesh` is the single mesh parser.
- CROSS-ENTRY migration — `migrate_overlay_entries` normalizes the oldest families (absolute ribbon
  `x`/`y`+`region_w`/`region_h` via `project::LegacyRibbonGeometry`, and top-left `u`/`v` via the PNG
  footprint passed by the caller) to modern `img_u`/`img_v` BEFORE per-entry decode. Both the typing
  loader and the doc loader run it. The absolute-ribbon family recovers a CHAPTER-WIDE scale from every
  page's aspect ratio, so it requires the FULL chapter page-size map — passing only the loaded page's
  size makes other pages default to a square aspect and corrupts the solve (and, because any doc edit
  flushes the page's text inline and then ignores `text_info.json`, that corruption is permanent).
- WRITE — `encode_transform_fields` (center→img_x/y, rad→`rotation_deg`) and `encode_deform_mesh`
  (`DeformRec`→`deform_mesh`) are the single serialization point for the disk vocabulary; the typing
  tab's `build_storage_overlay_entry` calls them (no hand-rolled deg/mesh serialization remains).
`ensure_page_loaded(page_idx, primary, fallback, legacy_text_dir, page_sizes)` takes the FULL chapter
page-size map `&HashMap<usize,[usize;2]>` (the loaded page's size is `page_sizes[page_idx]`; the whole
map feeds the ribbon solve). Callers build it: typing from its memoized page-image-dimension cache
(`page_sizes_map`), PS from `project.pages` image dimensions (memoized, the loaded page seeded from the
stack size).

### Lock-free decode split (off-thread page load)
`ensure_page_loaded` is a thin composition of two parts so the page-switch PNG decode can run off the
GUI thread WITHOUT holding the shared doc lock across a multi-MB decode:
- `decode_page_payload(page_idx, primary, fallback, legacy_text_dir, page_sizes) -> Result<DecodedPagePayload, String>`
  — a PURE associated fn (no `&mut self`). It does ALL the disk I/O + PNG decode + legacy migration and
  returns the OWNED `DecodedPagePayload` (the page's raster + text `LayerNode`s, already z-sorted and
  re-ranked, plus the raster groups). It still REQUIRES the FULL chapter `page_sizes` map (the
  absolute-ribbon legacy migration recovers a chapter-wide scale from every page's aspect — a partial
  map corrupts geometry). `DecodedPagePayload`/`LayerNode`/`ColorImage`/`Value` are `Send`, so the
  payload can be built on a worker thread.
  - `legacy_text_dir: Option<&Path>` is the un-migrated legacy `text_images/` dir, appended as the LAST
    text source after `primary`/`fallback` (so a migrated `layers/text_info.json` still wins), or `None`
    once the chapter is migrated. It feeds BOTH the `text_info.json` read and the overlay-PNG lookups, so
    a never-migrated legacy chapter's text becomes visible in the shared doc (hence the PS editor, whose
    only text source is the doc). A uid-less legacy entry gets a DETERMINISTIC uid via
    `text_payload::stable_overlay_uid` (UUIDv5 of the PNG file name), matching the typing loader so the
    same overlay never double-renders. GATE (both tabs): the legacy dir is threaded ONLY when
    `migrate::manifest_has_inline_text(committed_layers_dir)` is false, computed once per chapter and
    cached (typing: `doc_legacy_text_dir` set in `render_jobs::ensure_loader_started`; PS:
    `doc_legacy_text_dir_cache` in `request_page`) — a migrated chapter passes `None` so a stale
    `text_images/text_info.json` can never resurrect a deleted overlay.
- `insert_decoded_page(&mut self, page_idx, payload)` — takes the doc lock only to MOVE the payload into
  `self.pages` (no I/O). Memoized: an already-resident page discards the incoming payload (live in-memory
  edits are never clobbered) and does NOT bump the version; a real insert bumps it.
`ensure_page_loaded` = `decode_page_payload` + `insert_decoded_page` (unchanged behavior for the
synchronous callers and all tests). The PS page loader (`ps_editor/page_loader.rs`) calls
`decode_page_payload` lock-free on its worker and `poll_loader` calls `insert_decoded_page` under a brief
lock — so no doc lock is ever held across a decode.

Loads read the unsaved dir first, falling back to the main dir, for both the manifest and each PNG
(mirroring `text_images/`). Unmaterializable nodes (text/group) and missing/size-mismatched PNGs
are skipped with a warning rather than failing the load.

## Format compatibility (forward-only)
All `layers.json` reads go through `compat::read_manifest`, the **single** home for old-format
handling, so the rest of the code only ever sees a canonical, current-version `LayersManifest`.
A read parses the file as untyped JSON, inspects `schema_version` (absent ⇒ v1), and either reads
best-effort + warns (file is *newer* than supported) or runs the forward migration chain
`migrate_value` (file is *older*) and **re-stamps** it to the current version. Re-stamping matters:
a forward-only write then records the current version, instead of silently re-emitting the old one.
Compat is the only reader of retired/renamed fields — the canonical structs in `manifest.rs` carry
no compat-only concerns, so migrating a legacy field can never corrupt a current parameter. Adding a
version: bump `LAYERS_SCHEMA_VERSION`, add a `migrate_vN_to_vN1` step, chain it, then drop the
now-retired `#[serde(default)]` from the canonical struct (the migration is its only reader).

## Files
- `manifest.rs` — serde schema (`LayersManifest`, `PageLayers`, `LayerRec`, `LayerKindRec`,
  `TransformRec`, `DeformRec`, `GroupRec`, `PayloadRef`).
- `compat.rs` — isolated backwards-compatibility: `read_manifest` (raw JSON → version-migrated
  canonical manifest) and the `migrate_value` forward-migration chain. `persist::read_manifest`
  delegates here.
- `migrate.rs` — eager one-shot chapter migration (`chapter_needs_migration` / `migrate_chapter_to_v3`)
  that converts a legacy `text_info.json` chapter to v3 inline on disk, renaming overlay PNGs (pixels
  preserved) and `.bak`-ing `text_info.json` last. Triggered in the background on chapter open (see the
  Eager migration section). Pure file/manifest ops — no UI/render deps.
- `persist.rs` — save/load for both node kinds, with a process-wide `MANIFEST_LOCK` so the two
  writers never corrupt the shared `layers.json`:
  - Raster nodes + groups (PS editor): `save_page_rasters` / `load_page_rasters`, PNG IO, orphan
    pruning. Preserves existing text nodes on rewrite. The PS editor is authoritative only over the
    rasters it actually loaded into its stack: a manifest raster whose uid is **not** in the saved
    `layers` is preserved verbatim (effects + PNGs) — it belongs to another tab (e.g. the typing tab
    added/effected it while the PS stack was stale or never loaded it). Only uids passed in
    `removed_uids` (rasters the PS editor explicitly deleted/merged-away this session) are dropped and
    pruned. This is what stops a "save to project" whole-page flush from wiping the typing tab's
    rasters and their non-destructive effects.
  - Targeted single-raster ops (both tabs, e.g. the typing tab adding/moving/effecting an external
    image as a raster without rewriting the whole page): `add_page_raster` (append one node + PNG on
    top), `update_raster_transform` (geometry only, no PNG), `update_raster_geometry` (transform + deform
    mesh together — the typing raster perspective transform mode), and `update_raster_effects`
    (non-destructive: sets the `effects` chain + writes/clears the `rendered_file` PNG, leaving
    `base_file` intact so effects are reversible across restarts). All preserve every other node/group.
    A raster's `mask_clip` flag (typing tab; **rasters default OFF**) round-trips through `LayerRec.mask_clip`
    and `RasterLayerOut`/`RasterLayerIn`; `save_page_rasters` PRESERVES an existing on-disk `mask_clip`
    when the writer passes `None` (e.g. the PS editor, which has no mask-clip), so it is never clobbered.
    `LayerDoc::flush_page_dropping_raster` flushes the page DROPPING a removed raster uid (so a deleted
    raster does not resurrect — `save_page_rasters` otherwise preserves an unowned manifest raster).
  - Save-to-project layers merge: `merge_unsaved_layers_into_committed(committed_dir, unsaved_dir,
    owned_text_pages)` merges the unsaved staging `layers.json` INTO the committed one PER PAGE, and
    `app::merge_unsaved_into_project` calls it instead of a file-level overwrite of `layers.json`.
    OWNERSHIP, two axes: (1) a committed-only page (absent from unsaved) is PRESERVED entirely (**ВВД/13
    truncation fix** — the doc session's unsaved manifest only holds the pages the user visited, while
    the committed manifest may carry MORE pages e.g. all of them written by the eager migration; a blind
    overwrite DROPPED the committed-only pages). (2) For a page in BOTH, rasters/groups take the unsaved
    version, but TEXT takes the unsaved version ONLY when the page is in `owned_text_pages` (the doc
    LOADED its text this session, so the unsaved text incl. DELETIONS is authoritative); otherwise the
    committed TEXT nodes + text-group bands are PRESERVED. **Symmetric text-drop fix**: a PS raster-only
    edit (band reorder / grouping / raster delete) on a page whose text was never loaded writes a
    text-less staging page — without the ownership guard the whole-page replace would DROP the committed
    text; a naive "preserve absent text" would RESURRECT a legitimately-deleted text. The owned set
    comes from `TypingTabState::flush_text_layers`, which now flushes text for EVERY doc-resident page
    (`LayerDoc::resident_pages`) — making staging text-complete for owned pages — then subtracts pages
    whose latest TEXT saver write FAILED at the merge barrier, so committed text is preserved fail-safe
    without coupling text ownership to raster/effects outcomes.
  - **Non-destructive raster effects model**: a raster keeps `base_file` (original), an `effects`
    chain, and a cached `rendered_file` (the post-effects PNG). `load_page_rasters` returns the
    DISPLAY image (`rendered_file` when effects present, else base) plus the `base_file` name + chain.
    `save_page_rasters` takes a `pixels_dirty` flag per `RasterLayerOut`: a non-dirty raster preserves
    its base PNG + `rendered_file` + `effects` (so a PS whole-page save never wipes another tab's
    effects); a dirty one (PS paint/cut/merge/bake) rewrites the base and drops the chain (bakes it in).
  - Text nodes (single writer, schema v3): `write_page_text_payload(layers_dir, fallback_dir, page_idx,
    &[TextPayloadOut])` writes each text node's FULL payload (`render_data` + canonical
    `transform`/`deform` + the rendered PNG name in `rendered_file` + `mask_clip` + the optional
    `overlay_is_image` marker) inline into
    `layers.json`, so the file is self-sufficient for text. Kind-filtered preservation (rasters + PS
    groups + rebuilt text-group bands survive), and it carries each existing node's `layer_idx` +
    PS-owned pin/z/group fields. WRITE-keep-present invariant: a page that already EXISTED (in this
    manifest OR in the committed `fallback_dir`) and is emptied to nothing stays PRESENT-but-EMPTY rather
    than being removed — so a last-text deletion survives the load (per-page primary-else-committed
    fallback sees the empty primary page, no resurrection) and the owned-page merge (present in unsaved →
    whole-page text replace). Only a page that never existed ANYWHERE is omitted. `fallback_dir = None`
    for callers with no committed dir (e.g. migration, which never writes empty payloads). FULLY-MANUAL
    TEXT Z: `write_page_text_payload` now ALWAYS emits text `pinned`-with-explicit-Z and NEVER creates a
    `TextGroup` band (`rebuild_text_groups` retired; legacy groups are dropped on write, their members
    already flattened to per-text bands on read by `layer_doc::ensure_page_loaded`). `merge_preserved_text_fields`
    carries PS-owned `group_uid` / `pinned_by_group` across a typing-side rewrite, and preserves the
    explicit `z` ONLY from an already-pinned disk node (a PS/typing reorder authority) so a legacy
    unpinned node never clobbers the doc's freshly-flattened per-text Z — that is what makes the per-text
    reorder survive a later text flush and keeps existing chapters' visual order on first save. The reference-only writers
    (`save_page_text_nodes` / `replace_all_text_nodes`) were removed in Phase D; this is the only text
    writer. `load_page_text_nodes` returns
    an optional `TextNodeIn.inline` (`TextInlineIn`) when the node carries inline `render_data`; the
    doc load uses it to build a text node without `text_info.json`. `write_text_image` writes the
    rendered text PNG; `text_image_file_name` is its uid-keyed name. The doc's `flush_page` (whole
    page) and `flush_page_text` (text only, leaving rasters on disk untouched) drive this for every
    `NodeBody::Text`. The doc node carries `text_layer_idx` (the «Группа текста N» axis), authoritative
    so a flush persists it for NEW overlays too.
  - Unified grouping (PS editor): `save_page_grouping(GroupingEdit)` — one locked manifest RMW (no PNG
    IO) that creates/removes `GroupRec`s, sets `group_uid` on raster + text nodes, toggles collapse /
    group visibility/opacity, applies a complete band `order` (reusing `apply_band_order`), records
    group-owned pins (`pinned_by_group`), and prunes emptied text-group bands. A text put into a PS
    group is auto-pinned so it owns a `Band::PinnedText` Z and can sit anywhere in the group's
    contiguous run. The PS editor flushes rasters (`save_page_rasters`) before this so freshly-added
    raster layers already have nodes for the edit to land on.
- `effects.rs` — the "render type 2" seam: `apply_effects_to_color_image(&ColorImage, effects_json)`
  bridges egui `ColorImage` to the typing tab's pure `apply_effects_to_image`. Straight alpha both
  ways; effects may enlarge the canvas (shadow/glow), so center-placed callers must recenter.
- `saver.rs` — OFF-THREAD coalescing persistence for the doc (additive; does not change any persist
  write logic). `LayerSaver` owns a worker thread (`recv` + `try_recv` drain) that BUCKETS jobs per
  `page_idx`, keeping the LATEST data PER KIND (rasters / text / per-uid effects) — so a Full + a
  TextOnly job for the same page MERGE without dropping either kind. A `PageSaveJob` carries OWNED data
  (`OwnedRasterLayer` / `OwnedTextNode` mirror the `persist::RasterLayerOut` / `TextPayloadOut` inputs
  but own their `ColorImage`s; `EffectsSaveItem` mirrors `persist::update_raster_effects`), so the
  worker holds no doc lock; its `run` replays the EXACT `persist::save_page_rasters` →
  `write_page_text_payload` → `update_raster_effects` sequence of `LayerDoc::flush_page`.
  - The `effects` half is a TARGETED per-raster effects-only update (`Vec<EffectsSaveItem>`, latest
    wins per uid on coalesce). It never rewrites the page raster set and is the ONLY path that can
    express the CLEAR case (empty chain + `display_image: None`) — the whole-page raster reconcile loop
    skips empty chains. The PS/typing effects polls route through it via
    `LayerDoc::enqueue_raster_effects`.
  - `Barrier` (via `barrier_blocking`) waits until all prior jobs complete and returns a snapshot of
    pages whose latest TEXT write FAILED; `Shutdown` drains then stops. `LayerSaverHandle` is a cheap-clone
    `Sender` wrapper for a merge worker / app-close drain.
  WIRING: the doc enables the saver via `enable_background_saver` (called ONCE in `app.rs` on the
  shared doc at startup) and feeds it through `enqueue_page_save` / `enqueue_page_text_save` /
  `enqueue_raster_effects` (sync-flush fallback when no saver is enabled). PS per-edit/raster flushes
  and typing text flushes ENQUEUE. Dirty flags clear only after the frame-loop poll consumes a
  successful acknowledgement whose per-kind epoch still matches the latest edit/enqueue; failed or
  stale completions leave that kind dirty for retry. The save-to-project merge worker and the eframe `on_exit` /
  exit-cleanup paths `barrier_blocking` (and shut down) the saver so no enqueued write is lost. The
  barrier reports only latest text-write failures, so raster/effects failures do not revoke text
  ownership. NO
  barrier ever runs on the GUI thread — only in the merge worker and at teardown.

### Text migration gate (lazy, on read)
`ensure_page_loaded` treats a page as MIGRATED once it carries any inline text node (`write_page_text_payload`
always writes the page's FULL text set inline at once, so "any inline node" ⇒ "all text inline"). Inline
image overlays use `is_image: true` with `render_data: Null` and participate in the same gate. A migrated
page IGNORES `text_info.json` entirely, so an overlay deleted/rasterized from the inline set
does not resurrect from the stale legacy file. A page with no inline nodes is pure-legacy and reads
`text_info.json` (migration-on-read), migrating to inline on its first flush. This lazy path stays as
the per-frame safety net.

### Eager one-shot chapter migration (`migrate.rs`)
On chapter open the typing tab runs `migrate::migrate_chapter_to_v3` ONCE in the background (after the
initial overlay load, so it does not race the loader on PNGs). It converts a whole legacy chapter to
v3 persistently:
- `chapter_needs_migration(layers_dir, text_images_dir)` — idempotency is **on the TARGET**: if
  `layers/layers.json` already carries any inline (v3) TEXT node (`render_data`), the chapter is treated
  as MIGRATED and this returns `None` REGARDLESS of a lingering `text_info.json` in EITHER dir. Otherwise
  it needs migration iff a legacy `text_info.json` (committed `layers/` preferred — newer/complete —
  else legacy `text_images/`) carries ≥1 TEXT overlay. **This is the ВВД/13 incident fix**: the old
  predicate keyed only on `text_info.json` presence, so after the primary `layers/text_info.json` was
  migrated and `.bak`'d, a STALE secondary `text_images/text_info.json` re-triggered migration and
  overwrote the good v3 data with the partial stale set. The target-based gate makes a completed
  migration permanent. (Trade-off: a crash mid-migration that left SOME pages inline blocks eager
  completion of the rest; those pages still load via the lazy on-read path — no data loss.)
- For every page: cross-entry-migrate + decode geometry through the SHARED codec with the FULL chapter
  `page_sizes` map (ribbon correctness); for each text or image overlay **RENAME (move)** its original PNG
  into the v3 `text_image_file_name`, PRESERVING the bytes (never re-rendered); build the inline node
  (text render data, or `is_image: true` with `render_data: Null`, plus radians transform/deform,
  mask_clip, and renamed PNG) and write it via
  `write_page_text_payload` (preserving rasters / PS groups / pin-z-group). If the original PNG is
  genuinely missing the overlay is kept WITHOUT an image (logged) — never DROPPED.
- ORDER for rollback safety: the destructive `text_info.json → text_info.json.bak` rename happens LAST,
  only after all pages are written and PNGs renamed, so a crash leaves the legacy file intact and the
  migration re-runs. The rename retires **BOTH** locations (`layers/` AND `text_images/`), not just the
  chosen source, so no stale secondary can re-trigger. An existing `.bak` is never clobbered
  (`.bak.1`, `.bak.2`, …).
- When an unsaved staging `layers.json` already holds a migrated page (uncommitted PS edits), the
  migrated inline text is mirrored into it ADDITIVELY PER UID: every text node already inline in the
  unsaved page is KEPT verbatim (it is either already-v3 or a fresher live edit), and migrated nodes are
  added only for uids NOT already inline there. This is a data-safety requirement — a plain full
  replace would (1) DROP an overlay staged only in `_unsaved` (created, never saved-to-project → no
  `text_info.json` entry) and (2) CLOBBER a fresh edit flushed during the migration window. On
  completion the migrated doc pages are evicted so both tabs re-project the v3 data.
- KNOWN LIMITATION (by design): an overlay whose original PNG was ALREADY missing pre-migration becomes
  an inline node with `rendered_file: None`. The doc build skips imageless text nodes, so after the
  `.bak` rename such an overlay is invisible/uneditable in the tabs. This is NOT new data loss — its
  text + geometry survive in `layers.json`, and the legacy entry survives in `text_info.json.bak`. The
  image re-renders on the next text edit (re-rendering from `render_data` during migration would need
  the typing text-render engine, which the model layer does not have — out of scope).

## Roadmap
Phase 1 (done): persist PS raster layers. Phase A1–A3 (done): schema v3 inlines the text payload into
`layers.json`. Phase A4–A5 (done): the shared doc is the SOLE text writer — the typing tab and PS
editor route every text create/edit/move/rasterize through the doc and persist via `flush_page` /
`flush_page_text`; NOTHING writes `text_info.json` (it is read-only legacy, read only for un-migrated
chapters). Save-to-project flushes resident text pages so viewed legacy chapters migrate. Later:
per-text reorder via the up/down arrows; merge_down by band-Z; rasterize polish; migrate external image
overlays out of `text_images/` into `layers/`.
