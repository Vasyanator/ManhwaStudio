# ms-text-detect golden fixtures

Reference outputs of the Python text-detector postprocess (CTD, PaddleOCR det, Surya det), recorded
before that postprocess moved into this crate. The Rust ports (`ctd.rs`, `surya.rs`, the Paddle path of
`db.rs` + `glyph_mask.rs`) are parity-tested against them.

**Generated, never hand-edited.** Producer: `tools/make_text_detect_fixtures.py`, run as
`./venv/bin/python tools/make_text_detect_fixtures.py` from the repository root. It drives the REAL
backend postprocess with a fake network (the stored map stands in for the forward pass), so it only
works on a revision that still has the Python postprocess; on a later revision it exits with status 2
and the committed fixtures stay authoritative. `manifest.json` records the git revision, the library
versions and a sha256 of every file. Two runs on one revision and environment are byte-identical.

## Layout

```text
fixtures/
  manifest.json               generator, git revision, versions, per-case file hashes
  ctd/<case>/                 dark_on_light, light_on_dark, color_bg
  paddle/<case>/              dark_on_light, saturated_text, light_on_dark
  surya/<case>/               stretch_both, width_only, low_contrast
```

Every case directory holds `case.json` plus PNGs. All PNGs are 8-bit, non-interlaced, no alpha:

| file | engine | content |
|---|---|---|
| `input.png` | all | the page, RGB, `source_size` |
| `seg.png` | ctd | segmentation probability map, gray, `map_size` |
| `shrink.png` | ctd | DB shrink map (network `lines` channel 0), gray, `map_size` |
| `seg_source.png` | ctd | intermediate: `seg` resized to the source (cv2 `INTER_LINEAR`), the mask `refine_mask` starts from |
| `prob.png` | paddle | DB probability map, gray, `map_size` |
| `heat.png` | surya | channel-0 heatmap in processor space, gray, `map_size` |
| `proc_mask.png` | surya | intermediate: binary mask in processor space, before the nearest resize |
| `expected_mask.png` | all | expected binary mask (0/255), `source_size` |

**Map quantisation (binding):** a stored map level `k` was fed to Python as `float32(k) / float32(255)`.
A Rust test reproduces it exactly with `k as f32 / 255.0`. CTD's `postprocess_mask` computes
`trunc(p * 255)` and gets `k` back for every level (checked), so the stored u8 levels ARE what the CTD
refine sees.

**Surya `heat.png` is the stitched map AFTER the ×4 upsample** (the full processor-space map that
`_extract_mask_and_boxes` receives), not the network's H/4 output. Parity starts at `surya::postprocess`.

## `case.json`

Common keys (sorted, `indent=1`):

- `engine`, `case`, `description`.
- `source_size` / `map_size`: `[w, h]`. They differ in every Paddle and Surya case and in two CTD cases,
  so the map->source mapping is always exercised.
- `files`: the role -> file name map of the table above.
- `params`: the constants the reference used.
- `expected`: what the service returned to Rust:
  - `blocks`: `[{x1, y1, x2, y2}]` in source px. CTD gives floats with integral values, the other two
    give ints. Order is the service's: CTD sorts by `(y1, x1, y2, x2)`, Paddle keeps contour order,
    Surya sorts by its float bbox.
  - Paddle `polys`: `[{points: [[x, y] x4], score}]`. Surya `lines`: `[{polygon, bbox, confidence}]`.
- `intermediate`: optional ground truth for debugging a port, described per engine below.

Loading from a Rust test needs only `image` (a normal dependency, PNG enabled) and `serde_json`
(workspace dependency; add `serde_json.workspace = true` under `[dev-dependencies]`):

```rust
let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/ctd/color_bg");
let case: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("case.json"))?)?;
let page = image::open(dir.join("input.png"))?.to_rgb8();
let shrink = image::open(dir.join("shrink.png"))?.to_luma8();
```

## CTD (`ctd/`)

The real `TextDetector.__call__` (`detection/textdetector/ctd/inference.py`) plus the service step
`CtdTextDetectorService._detect_from_encoded_image_bytes` (`detection/ctd.py`). The network input is the
letterbox of the page to `detect_size`; its maps are the stored maps padded with zeros to
`detect_size²`, then cropped back to `map_size`.

- `params`: `detect_size`, `letterbox_ratio`, `letterbox_pad_right_bottom`, `db_thresh` 0.3,
  `db_unclip_ratio` 1.5, `db_max_candidates` 1000, `db_skip_sside_lt` 2, `score_filter_gt` 0.6,
  `mask_binarize_gt` 30, `mask_dilate_size` 0. The dilation is recorded as 0 on purpose: decision Q7(a)
  leaves the only dilation on the Rust side.
- Cases: `dark_on_light` (map = source, right padding), `light_on_dark` (letterbox UPSCALE: map 229x320 for
  160x224), `color_bg` (letterbox DOWNSCALE: map 199x256 for 224x288; white bubble, red and blue text on a
  gradient).
- `intermediate.db_candidates`, one per contour in `findContours` order:
  - `mini_box_map_px`, `sside_pre`, `skipped_sside_lt_2`;
  - `score`: mean of the map inside the CONTOUR polygon;
  - `unclipped_map_px`: integer pyclipper output;
  - `box_map_px`: the second min-area box; `sside_post`;
  - `kept_score_gt_0_6`.
- `intermediate.refine_blocks`, one per block in block order:
  - `block_xyxy`, `window_xyxy` (`enlarge_window`);
  - `topk.colors`, `topk.inrange_bounds` (unrounded), `topk.histogram_first_last_edge`,
    `topk.histogram_sample_count`, `topk.minxor` (`xor_sum`, `inverted` per colour),
    `topk.stable_sort_order_differs`;
  - `otsu.threshold_per_channel_bgr`, `otsu.minxor` per channel, `otsu.chosen_channel_bgr`;
  - `candidates_in_merge_order`: the xor sums after the stable sort.

Upstream behaviour the port must replicate (paths under `modules/ai_backend/detection/`):

1. **Grey from a BGR crop with RGB weights** (`textdetector/ctd/textmask.py:56`): the page is BGR (cv2
   decode), but `COLOR_RGB2GRAY` is applied. Grey = `(B*9798 + G*19235 + R*3735 + 16384) >> 15` (cv2 4.12
   8-bit fixed point, verified exact).
2. **Swapped histogram names** (`textmask.py:59-60`): `bin, his = np.histogram(px, bins=255)` makes `bin` the
   COUNTS and `his` the 256 EDGES, and `get_topk_color(his, bin)` is called with them. The "colours" are
   therefore float LEFT EDGES of 255 equal bins over `[min, max]` of the candidate pixels, not integer grey
   levels. numpy uses `[min-0.5, max+0.5]` when min == max. The candidate pixels are those where the window
   mask, eroded by a 3x3 square, is > 127 (`:58`).
3. **Unstable argsort** (`textmask.py:15`): numpy's default argsort orders tied counts arbitrarily (it is
   also CPU-dependent). The generator rejects any case where a stable sort would pick a different colour
   SET, or where only the order differs and two candidates tie on their xor sum. So a port using a stable
   sort must reproduce these fixtures exactly. On real pages the tie order can change the third colour, and
   no port can match that bit for bit.
4. **`inRange` with float bounds** (`textmask.py:64-66`): `c_top = min(colour + 30, 255)` and
   `c_bottom = c_top - 60` are floats. cv2 rounds each bound half-to-even (10.5 -> 10, 11.5 -> 12),
   saturates it to 0..255, and keeps `lo <= v <= hi`.
5. **Otsu channel order** (`textmask.py:41-52`): channels are tried in B, G, R order. `cv2.threshold(c, 1,
   255, OTSU|BINARY)` gives `v > t -> 255`. Python's stable sort keeps the LOWEST channel index on an xor
   tie. Each candidate also has its inverted form, chosen by `minxor_thresh` (`:27-39`), and on a tie the
   NON-inverted form wins. The xor sums are sums over 0/255 bytes.
6. **Connectivity really is 8** (`textmask.py:91,111`): `cv2.connectedComponentsWithStats(mask,
   connectivity, cv2.CV_16U)` passes its two extra arguments POSITIONALLY. They bind to the `labels`/`stats`
   OUTPUT parameters, so the connectivity is the default 8 and the label type is CV_32S. 8-connected labels
   are numbered by first pixel under the key `(y // 2, x)`, not in raster order. The merge result does not
   depend on label order: the components of one candidate are disjoint, and a component's xor delta touches
   only its own pixels.
7. **`merge_mask_list`** (`textmask.py:71-130`):
   - The window mask is eroded with the 3x3 ELLIPSE, which is a CROSS (`:85`). The erosion ignores the
     border.
   - It is then binarized `> 60` (`:87`).
   - A component is skipped when its bbox `w*h < 3` (`:95`). Otherwise it is kept if the xor against the
     prediction, summed over its bbox, decreases.
   - Then a 5x5 SQUARE dilate (`:109`).
   - Hole fill: components of the INVERTED merged mask. `area_thresh` is the second-largest area, and label
     0 is included (`:111-116`). Label 0 is the mask itself, so ORing it is a no-op.
8. **`enlarge_window`** (`textdetector/td_utlis.py:109-132`):
   - `delta = int(round(root / 2))`, where `root` is the larger root of `x² + (w+h)x - 1.5wh = 0` and
     `round` is Python's (half-to-even).
   - The expansion is clamped SYMMETRICALLY: `min(x1, im_w - x2, delta)`, and the same on y. A block near a
     border therefore barely grows on either side.
9. **DB** (`textdetector/db_utils.py:127-170`):
   - The score is taken over the CONTOUR polygon (`:153`), not over the min-area box.
   - Contours with `sside < 2` are skipped. The zero box they leave has score 0 and dies at the
     `score > 0.6` filter (`inference.py:258-260`).
   - Unclip (`:172-178`) uses the shapely area/length and pyclipper `JT_ROUND` (ArcTolerance 0.25).
     **pyclipper truncates float input coordinates toward zero** (probed), and returns integer paths. All
     returned paths are concatenated.
   - The coordinates are scaled by `source/map`, then `np.round` (half-to-even), then clipped to
     `[0, dest]` (`:166`).
10. **Seg resize and the final mask**:
    - The seg map is resized to the source with cv2 `INTER_LINEAR` (`inference.py:263`). It is stored as
      `seg_source.png`, so a port can separate resize differences from refine differences.
    - The final mask is `refined > 30` (`detection/ctd.py:406`).
    - Blocks are the min/max of the integer quad (`td_utlis.py:135-150`), then clamped, degenerate ones
      dropped, sorted and capped at 2500 (`detection/ctd.py:331-360`).

## Paddle (`paddle/`)

The real `PaddleTextDetectorService._detect_from_encoded_bytes` (`detection/paddle.py`). A fake runtime
runs the real `DBPostProcess` (`engines/paddle_onnx.py`), configured by `parse_det_config(None)`, on the
stored map. `map_size` is what the real `resize_image_for_det` gives the page, so it is a multiple of 32
and the aspect is stretched.

- `params`: `db_thresh` 0.3, `db_box_thresh` 0.6, `db_unclip_ratio` 2.0, `db_max_candidates` 1000,
  `db_min_size` 3.
- `intermediate.db_candidates`, one per contour, holds:
  - `mini_box_map_px`, `short_side_pre`, `skipped_lt_min_size`;
  - `score`, `skipped_lt_box_thresh`;
  - `box_map_px`, `short_side_post`, `skipped_post_lt_min_size_plus_2`.
- `intermediate.glyph_branches`, one per poly: `mean_sat`, `sat_fill` / `dark_fill`, and `branch`
  (`saturation` / `dark` / `light`). All three branches occur across the cases.

Upstream behaviour:

1. **The score is taken over the min-area QUAD**, not the contour (`engines/paddle_onnx.py:234-248`).
   The float corners go through `astype(np.int32)` (TRUNCATION) before `fillPoly` (`:247`).
2. **Unclip** (`:205-216`) takes the cv2 area and arc length, and pyclipper truncates the input toward
   zero. Only `expanded[0]` is kept.
3. **Coordinates** are `np.round(x * dest/map)` (half-to-even), clipped (`:198-199`).
4. **Glyph mask** (`detection/paddle.py:38-89`), per poly over its bounding-rect ROI:
   - The fills divide by the ROI area `h*w`, not by the polygon area (`:48,55`).
   - HSV saturation is the cv2 8-bit `S = ((max-min) * round((255<<12)/max) + 2048) >> 12` (0 when
     max = 0, verified exact).
   - Grey is the correct BGR->grey `(R*9798 + G*19235 + B*3735 + 16384) >> 15`.
   - The ROI result is closed with the 3x3 ELLIPSE (a cross, `:86`), then ORed into the page mask.
5. **Blocks** use floor/ceil, then clamp, then drop degenerate boxes, with no sort (`:177-183`).

## Surya (`surya/`)

The real `SuryaTextDetectorService._detect_with_predictor` (`detection/surya.py`). A fake predictor
yields `heat.png` as the stitched channel-0 heatmap. The page is decoded by PIL as RGB.

- `params`: `text_threshold` 0.6, `low_text` 0.35, `typical_top10_avg` 0.7, `y_expand_margin` 0.05,
  `min_component_area` 10. They come from `surya/settings.py` of surya 0.17.1, and `manifest.json`
  repeats them.
- Cases:
  - `stretch_both`: heat 256x256 for a 240x180 source. The thresholds saturate at their configured
    values.
  - `width_only`: heat 240x320 for a 200x320 source.
    - A frame-shaped component with a contained blob, which `clean_boxes` drops.
    - A square rotated 30 degrees in heat space, which takes the axis-box rule.
    - A weak line under `text_threshold`, and a component with area < 10.
  - `low_contrast`: heat 256x200 for a 180x240 source. The top-10 % mean is below 0.7, so both thresholds
    scale down unclipped.
- `intermediate`:
  - `dynamic_text_threshold_f32` / `dynamic_low_text_f32`: float32 values printed as doubles. Compare
    them within 1e-6.
  - `components`: every 4-connected component of `heat > low_text`, in label order. Each has `label`,
    `bbox_xywh`, `area`, `max` and `outcome` (`area_lt_10` / `max_lt_text_threshold` / `kept`).
  - `label_count`, `max_confidence`.
  - `proc_boxes`: quads in heat px after the roll, before rescale. `proc_confidences` are normalised by the
    maximum.

Upstream behaviour:

1. **Dynamic thresholds are float32** (surya `detection/heatmap.py:13-23`):
   - Take the mean of `np.partition(flat, n_top)[n_top:]` with `n_top = int(0.9 * n)`. numpy sums it
     pairwise in float32.
   - The factor is `sqrt(clip(mean / 0.7, 0, 1))`.
   - The results are clipped: `low_text` to `[0.1, 0.6]` and `text_threshold` to `[0.15, 0.8]`.
   - The comparisons `heat > low_text` (`detection/surya.py:393`) and `max < text_threshold` (`:438`)
     happen in float32. Note that 0.6f32 IS level 153/255, so a component whose max is level 153 is
     KEPT. The generator ensures that every SCALED threshold is at least 1e-5 away from every u8 level.
2. **Components are 4-connected** (`:394-396`), with labels in raster order of their first pixel. A
   component is skipped when its stats area < 10 (`:405`).
3. **The window** is `[x - niter - 1, x + w + niter + 1)` clipped to the map, with
   `niter = int(sqrt(min(w, h)))` (`:422-430`). Only the pixels of THIS label in the window are used.
4. **The dilate** uses `MORPH_RECT` with `ksize = max(1, 1 + niter)` (`:442-444`) and the default anchor
   `ksize // 2`. With an EVEN ksize, the dilation extends right and down by `ksize/2` but left and up by
   only `ksize/2 - 1` (probed). It is clipped to the window. The dilated pixels go into the processor mask.
5. **The box**:
   - It is `minAreaRect` over the integer coordinates of the dilated pixels, then `boxPoints` (`:453-454`).
   - When `|1 - long/(short + 1e-5)| <= 0.1`, it is replaced by the axis box
     `[minx, miny]..[maxx, maxy]` with no +1 (`:456-467`).
   - It is rolled so that `argmin(x + y)` comes first; on a tie the first index wins (`:469-470`).
   - Confidence = component max / the max over the accepted components (`:475-476`).
6. **Rescale** (`:277-278`; surya `common/polygon.py:59-81`):
   - Each corner becomes `int(c * src/heat)`, which truncates.
   - Then `fit_to_bounds([0, 0, W, H])`; the bounds are inclusive of W and H.
7. **`clean_boxes`** (surya `common/util.py:11-38`):
   - Drops a box with zero width or height.
   - Drops a box whose bbox lies inside another one; the comparisons are inclusive.
   - Skips the comparison when the other box has an identical polygon or an identical bbox, so duplicates
     both survive.
8. **Y-expand** (`detection/surya.py:281-287`; `polygon.py:100-113`):
   - Applied only when `height < 3 * width`.
   - Each corner moves by `0.05 * height` in y, then `int()`. Corners 0 and 1 move up, 2 and 3 move down.
   - Then `fit_to_bounds` again.
9. **Mask** (`:289-293`): cv2 `INTER_NEAREST` resize of the processor mask, which picks
   `src = floor(dst * src_len / dst_len)` (probed). This differs from `image`'s pixel-centre `Nearest`.
10. **Blocks** (`:304-327`) are sorted by the float bbox `(y1, x1, y2, x2)`, then `int()`, and degenerate
    boxes are dropped.

## Regenerating

Only while the Python postprocess still exists. The generator itself enforces the following:

- It checks every label numbering it observes against the order rules above, and records the count in
  `manifest.json` `checks`.
- It rejects any case whose DB score lies within 0.02 of its threshold, or whose glyph-mask decision lies
  close to its boundary.
- It fails if a required branch is no longer exercised (`check_coverage`).

Commit the regenerated files together with the generator change that caused them.
