# Module: src/page_ops

## Purpose
GUI-free engine for STRUCTURAL page operations on a loaded chapter: move a
page, insert pages (from image files or a generated blank), delete pages,
stitch several pages into one composed page, split one page into several
ordered parts, rotate and crop one page in place. An
operation is executed on disk as a journaled, crash-safe transaction that keeps
every page-keyed artifact consistent in BOTH trees — the committed chapter dir
and the sibling `{chapter}_unsaved` staging mirror (the save flow copy-merges
unsaved over committed without deleting, so a tree remapped on only one side
would resurrect stale files on save).

## Architecture
```text
execute_page_op(paths, pages, op)             recover_pending_page_op(project_dir)
        |                                                   |
   fs_exec::execute                                  fs_exec::recover
        |  scan_chapter  -> plan::ChapterSnapshot           |
        |  plan::build_plan (pure) <- json_remap (pure)     |
        |                                                   |
        v                                                   v
  journal A -> phase A + per-rename dir fsync -> journal B -> phase B -> slots deleted
  (write plan)          (reversible)            (commit pt)  (idempotent)
```
- `plan.rs` is pure: permutation math + a journal-serializable action plan
  built from a snapshot struct. `json_remap.rs` is pure: `Value`-level rewrites
  of the page-keyed JSON documents. All filesystem work lives in `fs_exec.rs`.
- Phase A stages created files and renames every affected file to a unique
  temp (`__ms_pageop_{id}_{n}.mstmp`) in its own directory — fully reversible.
  Created files include COMPOSED rasters (a stitch), CROPPED ones (a split) and
  ROTATED + CROPPED ones (a crop): they are staged BEFORE any rename, so the
  compose step reads the source pages at their original paths.
  Recovery never re-composes and never re-copies an insert source; a missing
  staged file fails the transaction closed.
- The journal uses two durable slots. Phase A is stored in
  `{chapter}/page_ops_journal.json`; after all staged files and every phase-A
  rename have been file/directory-fsynced, phase B is created separately as
  `page_ops_journal.b.json`. Only then is A removed. Thus Windows replacement
  never needs a remove-then-rename gap, and at least one complete plan exists
  throughout the commit transition. Recovery trusts B when both slots exist,
  rolls BACK A, and rolls FORWARD B. It validates all journal paths and
  conflicts before touching the filesystem, fails closed when a required
  transactional artifact is missing, and never re-reads an external insert
  source. A failed rollback retains A for the next recovery attempt.
- Rewritten JSON bodies are computed at plan time and stored IN the journal,
  so roll-forward never re-reads half-moved inputs.
- Deleted page artifacts are moved (never destroyed) into
  `{chapter}/.pageop_trash/{unix-millis}/` preserving their title-relative
  structure; removed JSON entries are archived next to them
  (`deleted_bubbles.json`, `deleted_text_info.json`,
  `deleted_layers_pages.json`). Trash folders are never garbage-collected by
  the engine — cleanup is a manual/user action.
- `deleted_layers_pages.json` is ONE array of page entries whatever removed
  them, each carrying the page's ORIGINAL `img_idx`: a page deleted whole
  contributes its entry verbatim, and a page that SURVIVES but loses part of its
  tree (a split's deleted part, a crop's removed frame) contributes an entry
  holding just the dropped records plus the group / text-group records those
  layers claimed. This archive is not optional bookkeeping: only a dropped
  layer's rendered PNG goes to the trash, so the record is the sole surviving
  copy of its transform, its group membership and — for a TEXT node — the typed
  text itself.

## Files and submodules
- `mod.rs`: pinned public surface — `PageOpKind`, `PageOpOutcome`,
  `PageOpError`, `execute_page_op`, `recover_pending_page_op`.
- `plan.rs`: permutation math, canonical page-keyed file-name helpers (with
  citations to the owning modules), snapshot types, plan types, `build_plan`,
  and the pixel-identity geometry: `PlacementMap` (the ONE affine from a page
  to an output canvas, shared by all three ops), `StitchGeometry`,
  `SplitGeometry` + `SplitTreeRouting`, `CropGeometry` + `CropTreeRouting`, and
  the `PageGeometry` enum every planner takes. `PageGeometry` exposes NO
  per-variant accessor on purpose: an accessor would be exhaustive in one place
  only, so a new variant would read as "no geometry" in every planner and be
  silently ignored. Every consumer destructures it with its own exhaustive
  `match`, so a new variant fails to compile at each site that must reconsider
  it.
- `crop_geometry.rs`: the page-ROTATION model `PlacementMap` composes in front
  of its crop — `PageRotation` (quarter turns + a fine straightening angle) and
  `RotatedPage` (canvas size, the two point mappings, crop legality). Pure and
  GUI-free, and `pub(crate)` as a module so the page-manager UI imports the same
  formulas instead of restating them.
- `json_remap.rs`: bubbles / layers-manifest / text_info / detection-blocks
  rewrites over `serde_json::Value` (unknown fields survive), plus the
  geometry mapping and document merging of a stitch, the routing +
  partitioning of a split, and the survival test + dropping of a crop.
- `fs_exec.rs`: chapter scanning, journal I/O, phase A/B execution, recovery,
  durability helpers, integration + crash-recovery tests.

## What is remapped (and what deliberately is not)
Remapped, in committed AND unsaved trees unless noted:
- `src/{stem}.{ext}` — renamed onto the canonical stems of the NEW order
  (`{idx:03}`, same format as `project::normalize_page_filenames`, so the next
  load's normalize pass is a no-op); extension preserved. Committed only
  (unsaved has no `src/`).
- `clean_layers/{stem}.png` — follows its page's stem.
- `layers/*.png` (`ps_p{page:04}_...`) — renamed to the new page prefix. The
  prefix is load-bearing: `layer_model/persist.rs::prune_orphan_pngs` prunes
  by it, so a stale prefix would let a save of the page now holding the old
  index delete another page's PNGs.
- `layers/layers.json` — `PageLayers.img_idx` remapped, `base_file` /
  `rendered_file` references rewritten by each NAME's embedded index, pages
  kept sorted by `img_idx`.
- `translation_bubbles.json` — `img_idx` remapped; bubbles of deleted pages
  removed (archived); page-crop `crop_page_idx` remapped, and when the crop
  TARGET page is deleted the `crop_page_idx`/`crop_rect` keys are removed so
  the bubble degrades to a plain image bubble instead of cropping a wrong page.
- `text_info.json` (legacy typing metadata; checked in `layers/` and
  `text_images/` of each tree) — `img_idx` remapped; deleted pages' entries
  removed (archived) and their referenced overlay PNGs (+ `*_layout.png`
  companions) moved to the trash.
- `text_images/mask_page_{idx}.png` — renamed by index.
- `text_detection/{idx:05}_blocks.json` + `{idx:05}_mask.png` — committed only
  (`text_detection/` has no unsaved mirror in `ProjectPaths`); the `mask_file`
  field inside a parsed blocks file is rewritten when it names the per-page
  default mask. An unparseable blocks file is renamed opaquely (a dangling
  custom `mask_file` degrades gracefully on load: missing file -> empty mask).

A STITCH (`PageOpKind::Stitch`, N pages -> one page at `primary = min(idx)`)
keeps every rule above for the pages it does not touch and, for the merged
pages, replaces "rename" with "compose / merge":
- `src/{stem}.{ext}` — all sources are moved to the trash and ONE composed PNG
  is staged in their place (always PNG, whatever the sources were), painted in
  page order over the requested background.
- `clean_layers/{stem}.png`, `text_images/mask_page_{idx}.png` — page-sized
  rasters, so they are composed the same way, over a transparent (clean) or
  opaque-black (mask) canvas; a source page without one contributes nothing.
  No file is created when no source page had one.
- `layers/*.png` — still only renamed onto the merged page's prefix. Two merged
  pages carrying the same layer uid would collide on one file name; uids are
  UUIDs, and the collision is refused with `InvalidOp` instead of overwriting.
- `layers/layers.json` — the N page entries become ONE: `tree`, `groups` and
  `text_groups` concatenated in ascending page order (first page at the
  BOTTOM), `z` re-ranked densely across the merged bands, `layer_idx` re-based
  per page so the typing tab's text groups stay distinct. The re-basing offsets
  are computed chapter-wide (both trees' manifests AND every `text_info.json`)
  so all documents agree.
- `translation_bubbles.json` / `text_info.json` — nothing is dropped; entries of
  merged pages have their geometry mapped through their page's `PlacementMap`
  (page-normalized uv renormalized onto the canvas, absolute page px
  translated+scaled, layer-image-local values untouched) and `side` re-derived.
  A `crop_rect` is mapped through the CROPPED page's placement, not the
  bubble's own. When `crop_page_idx != img_idx` and both pages are stitched,
  this preserves the visible crop but moves the image-area rect and text areas
  to the crop page's placement on reload; the operation emits a runtime warning.
- `text_detection/` — merged into one document (blocks mapped, `source_size` and
  `mask_size` set to the canvas, masks composed) ONLY when every stitched page's
  document is trustworthy: valid JSON, `source_size` equal to the page image,
  and any mask not downscaled. Otherwise the stitched pages' detection files are
  moved to the trash with a `runtime_log` warning — deliberate, documented
  degradation of regenerable data instead of a silent wrong remap.
- `alt_vers/` is not remapped (see below); when the chapter has any, the plan
  emits a `runtime_log` warning naming it, because an N->1 operation shifts its
  positional pairing.

A SPLIT (`PageOpKind::Split`, one page -> N parts along parallel cuts) is the
inverse and reuses the same machinery. Each geometric part `k` (0 = topmost /
leftmost) is a crop of the source page at `[x_k, y_k, w_k, h_k]` becoming a page
of exactly that size, i.e. a `PlacementMap` with `scale = 1`, `dx = dy = 0`.
`order[k]` is part `k`'s position among ALL parts (a permutation of
`0..cuts.len() + 1`) and `deleted[k]` drops part `k` instead of turning it into
a page. The SURVIVING parts, ranked by their `order` value among themselves,
take the consecutive new indices `page_idx ..`, so they occupy a contiguous run
starting at the source page's own index and every later page shifts up by
`kept - 1`; `old_to_new[page_idx]` is `page_idx` (the one representative the
permutation type can carry, always the first surviving part). Keeping exactly
one part is legal — geometrically a crop, and then nothing shifts. Deleting
every part, and a `deleted` whose length differs from the part count, are
refused with `InvalidOp`: emptying a chapter stays the `Delete` planner's single
rule. Both validators (`split_permutation` for the index math and
`resolve_split_parts` for the geometry) share `validate_split_routing` so they
cannot disagree about what is legal.

What is CUT vs. what MOVES WHOLE:
- CUT, one crop per part: `src/{stem}.{ext}` (always re-encoded as PNG,
  whatever the source was), `clean_layers/{stem}.png`,
  `text_images/mask_page_{idx}.png` and — only when trustworthy —
  `text_detection/{idx:05}_mask.png`. The crop equals the destination, so the
  pixels are copied bit-exactly (no resampling anywhere in a split). The
  originals go to the trash.
- MOVE WHOLE: every LAYER. A layer crossed by a cut is never split; it goes to
  ONE part and its geometry is mapped through that part, so it may legitimately
  hang off the new page's edge (negative or over-size coordinates).

Routing rules (each entry must land on exactly ONE part):
- **A DELETED part is a `Delete` of that part.** `SplitGeometry::part_new_idx`
  returns `None` for it, and that `None` is the engine-wide signal "nothing of
  this part becomes a page": no raster is staged for it, no detection document
  is written, its bubbles go to `deleted_bubbles.json`, its `text_info` entries
  to `deleted_text_info.json` with their overlay PNGs (and `*_layout.png`
  companions) trashed, its layer records are removed from the manifest into
  `deleted_layers_pages.json` with the PNGs only they reference trashed, and a
  page-crop link into it is removed exactly as for a deleted page. Nothing routed to a deleted part is ever
  relocated onto a surviving one — the plan warns how many parts were dropped
  and what that archived. Every fallback that used to point at "part 0" or at
  the source page's index must point at `first_kept_part` /
  `first_kept_new_idx`, so an unroutable entry cannot vanish into a deleted
  part.
- **Layers — the exact-area rule.** The part holding the largest share of the
  layer's on-page AREA wins; an exact tie goes to the TOP part (horizontal cuts)
  or the LEFT part (vertical cuts) — geometric position, never user order. The
  footprint is a real polygon clipped against the cut half-planes and compared
  by shoelace area, not a bounding box: a `deform` mesh when the node has one
  (the mesh OVERRIDES the transform), otherwise the four `local_to_world`
  corners of the layer image. A mesh is measured CELL BY CELL — the absolute
  area of every grid quad, summed per part — never from its outer boundary
  ring: a user can FOLD a mesh in the typing tab and a self-intersecting ring's
  lobes cancel in the signed shoelace sum, so its area is not the filled area.
  A TEXT node stores
  `image_size: None`, so its size is PROBED from `rendered_file` at scan time;
  when the probe fails the node falls back to its transform CENTRE point and the
  plan warns — a documented degradation, never a silent violation.
- **Layer PNGs.** One page's PNGs fan out onto DIFFERENT `ps_p{page:04}_`
  prefixes, which the index embedded in the name cannot express, so a per-tree
  `uid -> part` / `file -> new page index` routing decides. A PNG no layer
  record claims follows the first SURVIVING part, with a warning; a PNG claimed
  only by records on deleted parts is trashed, and a claim from a surviving
  record always outranks one from a deleted record (that record still needs the
  pixels). ONE file claimed by records routed to DIFFERENT surviving parts is REFUSED with
  `InvalidOp` naming the file and the two parts: a file can only move to one
  prefix, so any answer would leave a record pointing at a PNG owned by another
  page, and `prune_orphan_pngs` prunes by that prefix. There are no shared-file
  semantics to guess — the same refusal the stitch applies to a duplicate uid.
- **Layer manifest.** The page entry becomes one entry per part that holds a
  layer. `z` is a per-page band axis, so it is re-ranked densely inside each
  part. A `GroupRec` / `TextGroupRec` whose members land on several parts is
  DUPLICATED into each of them (group uids are page-scoped — every `LayerDoc`
  group operation takes a `page_idx`), and omitted from parts holding no member.
  `layer_idx` is NOT re-based: different parts are different pages.
- **Bubbles.** A bubble ALWAYS goes to the part containing its ANCHOR point,
  including an image bubble whose visible area may mostly lie elsewhere (a
  user-fixed rule).
- **A page-crop bubble whose `crop_page_idx` is the split page.** The crop link
  is preserved, not dropped: `crop_page_idx` is remapped to the part holding the
  majority of `crop_rect`, the rect is renormalized into that part and clamped
  back into `[0, 1]`, and a clamp that actually trims is warned about. Dropping
  the link would silently degrade the bubble to a plain image bubble. The one
  exception is a crop of a DELETED part: there is no page to point at, so the
  link is removed, as for a deleted crop page.
- **Legacy `text_info.json`.** Routed by its deform mesh's cell area when it
  has one, otherwise by its decoded centre point: this document does not record the
  overlay's extent, so an area test is impossible here. In a v3 chapter the
  authoritative record of the same overlay is the `layers.json` node, which IS
  judged by the exact-area rule.
- **Detection.** The stitch's trustworthiness gate (`source_size` equal to the
  page image, mask not downscaled) PLUS a routability gate: every block must be
  an object carrying numeric `x1`/`y1`/`x2`/`y2`, because a split partitions the
  block list and a block no part can claim would be skipped by all of them and
  vanish. Both gates are all-or-nothing and are evaluated ONCE, before any
  per-part document is built; when either fails, the page's detection files go
  to the trash with a warning instead of being cut. When they pass,
  each part gets its own `{idx:05}_blocks.json` (blocks routed by the area of
  their rectangle, mapped into the part, `source_size` = `mask_size` = the part
  size) and its own cropped mask.
- `alt_vers/` warns for the same reason as a stitch: the page count changed.

A CROP (`PageOpKind::Crop`, one page rotated then cropped in place) keeps the
page COUNT and the whole page ORDER: `old_to_new` is the IDENTITY and no other
page's files move at all. The page is first mapped into its ROTATED CANVAS (the
axis-aligned bounding box of the rotated page, with the page centred in it —
`crop_geometry::RotatedPage`), and `rect` then selects the kept region of that
canvas, which becomes the new page size. Geometrically it is one `PlacementMap`
with `scale = 1`, `dx = dy = 0` and a NON-identity `PageRotation`, so every
helper is shared with the split verbatim.

EXACTNESS is the operation's central contract:
- with `angle_deg == 0.0` a crop is BIT-EXACT. The quarter turn is an integer
  pixel permutation (`image::imageops::rotate90/180/270`) and the crop copies
  pixels, so no float path and no re-encode artifact is involved. Every stored
  axis-aligned rectangle also maps to an exact axis-aligned rectangle, so none
  of the degradations below applies.
- a non-zero `angle_deg` RESAMPLES, by inverse-mapping every canvas pixel centre
  back through `RotatedPage::unmap_point` and sampling there. RGBA rasters are
  interpolated bilinearly on PREMULTIPLIED alpha and unpremultiplied afterwards
  (a straight-alpha blend pulls the meaningless colour of transparent pixels
  into the visible edge and halos it); MASKS use NEAREST NEIGHBOUR.
  Canvas area the rotated page does not cover is left fully transparent, so each
  category's own compose background shows through it (transparent for the page
  and clean overlay, opaque black for a mask).

Why masks are sampled with nearest neighbour — the answer, so nobody re-derives
it: BOTH mask families this engine moves are STRICTLY BINARY, by construction
and by contract, not merely thresholded on use.
- `text_images/mask_page_{idx}.png` is painted by
  `tools/mask_brush.rs::paint_binary_mask_segment`, written as `(v, v, v, 255)`,
  and re-thresholded at luma 128 by its loader (`tabs/typing/mask.rs`).
- `text_detection/{idx:05}_mask.png` comes from
  `tabs/translation/text_detector.rs`, whose `parse_mask_alpha_from_blob` and
  `glyph_mask_into_alpha` both normalize every byte to `0`/`255` with the comment
  "CTD mask is logically binary". Note the promotion rule is `!= 0 -> 255`, not a
  midpoint threshold, so ANY grey a filter introduces reads as fully masked and
  the mask grows outwards.
A smooth filter therefore does not blur such a raster, it changes what it MEANS.

THE POLICY, which holds for EVERY operation: **a mask raster is resampled
without interpolation at every step — the page-size pre-resize, the rotation and
the crop-to-destination resize alike.** It is carried by
`ComposeSource::nearest`, which sits on the SOURCE rather than on
`ComposeRotation` precisely so that one answer covers all three ops: a stitch and
a split never rotate, but they still reach the pre-resize whenever a mask file is
not page-sized, and smoothing it there breaks the same guarantee. The four mask
construction sites (`plan_typing_masks` and the detection planner, once per op)
set it; the page image and the clean overlay are continuous imagery and do not.

What is ROTATED AND CROPPED, all with the SAME transform so they stay exactly
the new page's size: `src/{stem}.{ext}` (always re-encoded as PNG), 
`clean_layers/{stem}.png`, `text_images/mask_page_{idx}.png` (nearest) and — 
only under a pure quarter turn — `text_detection/{idx:05}_mask.png` (nearest).
The originals go to the trash. `layers/*.png` are neither rotated nor renamed:
layer pixels are layer-local and the page prefix does not change.

Survival rules (each entry either MOVES WITH the page or is ARCHIVED; nothing is
relocated and nothing is silently dropped):
- an entry whose footprint still OVERLAPS the kept region survives and may hang
  off the new page's edge, exactly as a layer crossed by a split cut does;
- an entry lying ENTIRELY outside is archived the way `PageOpKind::Delete`
  archives a page's entries: bubbles into `deleted_bubbles.json`, `text_info`
  entries into `deleted_text_info.json` with their overlay PNGs (and
  `*_layout.png` companions) trashed, layer records removed from the manifest
  into `deleted_layers_pages.json` with the PNGs only they reference trashed.
  Every drop is warned about with a count.
- **Layers** follow the split's exact-area evidence order: a `deform` mesh
  measured CELL BY CELL (fold-correct), else the transform quad (which needs the
  image size — `image_size` for a raster, a probed `rendered_file` for a TEXT
  node), else the transform centre point with a warning. A record with NO
  placement evidence at all is KEPT, never dropped by a fallback.
- **Bubbles** are judged by their `rect_coords` BOX, falling back to the
  `img_u`/`img_v` anchor when the box cannot be read. This deliberately differs
  from the split's anchor-only rule: the split asks which of N parts owns a
  bubble, while a crop asks whether anything of it remains visible, and an
  anchor-only test would archive a bubble whose box still overlaps the frame.
- **Legacy `text_info.json`** is judged by its deform mesh's cell area when it
  has one, otherwise by its decoded centre point — this document does not record
  the overlay's extent. A missing position reads as the page CENTRE, a real
  position, so such an entry is archived when the crop removed the centre.
- **A page-crop bubble whose `crop_page_idx` is the cropped page** keeps its
  index (the page still exists at it); only `crop_rect` is mapped and clamped
  back into `[0, 1]`, and a trim is warned about. A rect with NOTHING left
  inside the kept region loses its link, as for a deleted page.
- **The manifest page entry always survives**, even when every one of its
  records was dropped: the PAGE still exists. `z` is re-ranked densely after a
  drop, and a `GroupRec`/`TextGroupRec` whose last member is gone is removed.
  `layer_idx` is NOT re-based: one page in, one page out.

STORED ANGLES gain the page's total rotation, which no other operation does:
`layers.json` `transform.rotation` (RADIANS, wrapped into `[-PI, PI)`) and
`text_info`'s `rotation_deg` (DEGREES, `angle` as its legacy alias, wrapped into
`[-180, 180)` — the same convention as
`tabs/typing/tab/geometry.rs::normalize_angle_deg`). The wrap matters because
crops COMPOSE: four quarter turns of one page would otherwise store a full turn
where zero means the same thing, and the number reaches the user as a degree
readout. Every reader goes through `sin`/`cos`, so a wrapped angle is
indistinguishable from the unwrapped one. Without this every layer would be drawn
unrotated on a rotated page. A record that stored no angle gains one, because
the readers default a missing angle to zero and zero is no longer correct. A
placement whose rotation is zero writes nothing at all, which is what keeps a
stitch and a split byte-identical.

FINE-ANGLE DEGRADATIONS, applied ONLY when `angle_deg != 0.0` and each reported
as a `runtime_log` warning naming what was degraded:
- a stored AXIS-ALIGNED rect (bubble `rect_coords`, `text_areas[].rect`,
  `crop_rect`) becomes the BOUNDING BOX of the rotated rect — the storage cannot
  hold a rotated rectangle;
- the page's `text_detection/` files go to the TRASH instead of being remapped:
  detection blocks are axis-aligned `x1/y1/x2/y2` and cannot describe a freely
  rotated page. The same all-or-nothing degradation of regenerable data the
  stitch and the split apply to an untrustworthy document, and it is decided
  BEFORE the trust gates, which have nothing left to judge.
Under a pure quarter turn NONE of these applies: detection is remapped (blocks
mapped exactly, blocks entirely outside dropped with a count, `source_size` /
`mask_size` set to the new page size), gated as a split's is by
`detection_merge_blocker` + `detection_rect_blocker`.

- `alt_vers/` warns even though the page COUNT did not change: the positional
  pairing survives, but the alternate version stays at the page's OLD size and
  orientation, so it no longer lines up with the page it belongs to.

Deliberately NOT touched (each with the reason):
- `alt_vers/` — alternate-version images pair with pages by SORTED POSITION
  inside each `alt_vers/<name>/` subfolder (`cleaning/tools/stamp.rs::
  source_path_for_page` indexes the sorted list), and their file names are
  arbitrary source names. There is no reliable per-file page key to remap, and
  renaming files would not change positional pairing. After a structural
  operation the stamp tool's alt-version alignment may shift — a known,
  documented limitation instead of a silent wrong guess.
- typing overlay PNG names in `text_images/` (`typing_overlay_p{page:04}_...`)
  — the page token is a creation-time uniqueness hint only; loading goes
  through the JSON `file` reference and the stable overlay uid is derived FROM
  the file name (`text_payload::stable_overlay_uid`), so renaming would sever
  `layers.json` references. Page association lives in `img_idx`, which IS
  remapped.
- `image_bubbles/` — media files are keyed by bubble id, not page. Files of
  bubbles removed with a deleted page remain as orphans; the archived
  `deleted_bubbles.json` keeps their `image_path` for manual recovery.
- `cleaned/` and `saved/` — legacy folders migrated/consumed at load before
  any page op can run (`reconcile_clean_layers_dir`,
  `ensure_clean_layers_dir` bootstrap); remapping inert legacy data risks more
  than it fixes. Known edge: if `clean_layers/` is later emptied by hand, a
  bootstrap re-copy from a stale `cleaned/` would restore pre-operation order.

## Contracts and invariants
- Worker-thread only: synchronous disk I/O and fsync — never call from the GUI
  thread. Callers must quiesce all chapter writers (layer saver, bubble flush,
  overlay autosave) before `execute_page_op` and reload the project after.
- Operations are applied to BOTH trees immediately; they are not staged and are
  not undone by discarding unsaved changes.
- `recover_pending_page_op` must run at the very start of project load
  (`ProjectData::load_internal`), before any reconcile/normalize pass reads
  chapter files. A failed recovery aborts the load; the journal is left in
  place for inspection/retry.
- The planner is pure and learns pixel sizes only through the snapshot, which
  `scan_chapter` fills ONLY for the operations that need geometry (stitch, split
  and crop): `ChapterSnapshot::page_sizes` is an image-header probe per page, and
  `TreeSnapshot::layer_png_sizes` an image-header probe per layer PNG of the
  page a split cuts or a crop reframes. Any of the three against a snapshot
  without page sizes fails with `InvalidOp`; a missing layer-PNG size only
  degrades that layer's decision to its centre point, with a warning. Both
  `scan_chapter` switches (`needs_sizes` and the layer-PNG probe) are exhaustive
  `match`es rather than `matches!`, because a new operation that forgets either
  one fails deep inside `build_plan` with a message far from the cause.
- Nothing this engine removes is destroyed: every removed JSON entry has an
  archive (`deleted_bubbles.json` / `deleted_text_info.json` /
  `deleted_layers_pages.json`) and every removed FILE goes to the trash. A
  removal path that writes no archive is a defect, not an optimization — a layer
  record in particular is not recoverable from its PNG.
- A stitch never deletes a page-keyed JSON entry, and neither does a split that
  keeps every part: merging and cutting preserve every entry, so the
  `deleted_*.json` archives stay empty for them and only FILES (the source page
  images and the rasters replaced by composed or cropped ones) go to the trash.
  A split with a DELETED part is the exception and behaves like a page delete
  for the entries routed to that part alone (see the split routing rules); so is
  a crop, for the entries the new frame removed entirely.
- Legacy un-migrated documents are rejected, not guessed: bubbles or text_info
  entries in the absolute-ribbon-coordinate format (no `img_idx`, numeric
  `x`/`y`) are keyed by ribbon position — which any page op changes — so
  `execute_page_op` fails with `InvalidOp` until a normal load has migrated
  them.
- The placement affine is `rotation -> crop -> scale -> translate`. A stitch and
  a split place with `PageRotation::IDENTITY`: the rotation step then returns its
  input bit-for-bit and every remapped value is exactly what the separable
  crop/scale/translate produced, which is what keeps those two pixel-identical.
  A CROP is the one rotating placement. It reads its `crop` in the ROTATED
  CANVAS' pixels, so `crop_rect()` stays an EXACT axis-aligned rectangle and a
  pixel consumer must first rotate the page into that canvas
  (`PlacementMap::rotates()` says when, `ComposeSource::rotation` carries the
  recipe). Three consequences a rotating placement carries, all implemented by
  the crop: the stored angles (`layers.json` `transform.rotation` in radians,
  `text_info` `rotation_deg` in degrees) gain
  `PlacementMap::rotation_radians()` / `rotation_degrees()`; an axis-aligned
  stored RECT (bubble `rect_coords` / `text_areas`, detection blocks) can only
  degrade to the bounding box of the mapped quad, so it is done only for a fine
  angle and warned about; and a HALF-SPECIFIED stored point (`img_u` without
  `img_v`, a block without `y1`) cannot be mapped on one axis at all
  (`map_x_without_y` returns `None`) and must be gated out by the operation —
  which is what `detection_rect_blocker` does for detection blocks.
- Uses `std::fs` directly, not the `crate::storage` seam: the transaction
  needs fsync and same-volume rename semantics the seam does not model. The
  feature is native-desktop; on wasm the journal never exists and recovery is
  an inert no-op. A web port of page ops requires extending the seam first.
- Windows: phase-B rename targets are guaranteed free (phase A vacated them),
  and A -> B uses distinct journal names, so no rename-over-existing is relied
  upon. Directory fsync is Unix-only best-effort (same policy as
  `tabs/settings`); each phase-A rename and staged create requests it before B.
- Same-volume assumption: temps live in their file's own directory and the
  trash lives inside the chapter dir, so every rename stays on one filesystem
  (`_unsaved` is a sibling of the chapter under the same title dir).

## Editing map
- New op kind or index-math change: `plan.rs` (`permutation_for_op`) +
  `mod.rs` (`PageOpKind`). A new PIXEL-IDENTITY op also needs a `PageGeometry`
  variant, which the compiler then demands at every planner and remap site, plus
  both `fs_exec::scan_chapter` switches (`needs_sizes`, the layer-PNG probe).
- New page-keyed artifact category: add it to `TreeSnapshot`/`ChapterSnapshot`
  + a `plan_*` function in `plan.rs`, scanning in `fs_exec::scan_tree`, and a
  rewrite in `json_remap.rs` if it is a JSON document.
- Journal format / crash-safety behavior: `fs_exec.rs` (bump
  `JOURNAL_SCHEMA_VERSION` on incompatible plan changes; it is at 3, raised from
  2 when `ComposeSource::rotation` was added for the crop, and from 1 when
  `NewPageContent::ComposedPng` was added for the stitch).
- Stitch / split geometry: `plan.rs` (`PlacementMap` is the single affine —
  never re-derive the formula at a call site; `SplitGeometry::part_for_*` is the
  single routing decision) + `json_remap.rs` for the per-document application.
- Page ROTATION (canvas size, the point mappings, crop legality): only
  `crop_geometry.rs`. The engine, the pixel executor and the UI preview all call
  it; a second copy of `|w·cosθ| + |h·sinθ|` anywhere is a defect.
- Crop geometry and survival rules: `plan.rs` (`resolve_crop` is the single
  legality rule, `CropGeometry::keeps_*` the single survival test) +
  `json_remap.rs` (`crop_layer_routing`, `layer_survives_crop`,
  `bubble_survives_crop`, `text_info_survives_crop`, `crop_page_layers`,
  `crop_detection_blocks`) for the per-document application.
- Crop PIXELS (rotation, resampling, mask sampling): `fs_exec.rs`
  (`rotate_page_raster` + `sample_nearest` / `sample_bilinear_premultiplied`),
  driven by the journaled `ComposeSource::rotation`.
- Split part DELETION: `plan.rs` (`validate_split_routing` is the single
  legality rule, `resolve_split_parts` the survivor ranking); every consumer
  branches on `SplitGeometry::part_new_idx` being `None` — grep it before
  adding a new per-part loop, and never default such a loop to part 0.
- Split routing of layers: `json_remap.rs` (`split_layer_routing`,
  `assign_layer_part`, `deform_mesh_cells`) + `plan.rs` (`SplitTreeRouting`,
  `SplitGeometry::part_for_polygon_group` — the fold-correct area sum).
- Split routing of detection blocks: `json_remap.rs`
  (`detection_merge_blocker` + `detection_rect_blocker` are the two gates,
  `split_detection_blocks` builds a part's document and fails closed on
  anything the gates should have caught).
- Canonical file-name formats: they are OWNED by other modules (see the cited
  helpers in `plan.rs`); change them there first, then mirror here.
