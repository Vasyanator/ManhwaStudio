"""
FILE OVERVIEW: modules/new_project/test_adv_fetch_cloak_cli.py
Focused tests for CloakBrowser advanced downloader helper contracts.

Main items:
- verifies page console warning/error diagnostics are forwarded to daemon logs.
- verifies deep-capture DOM update collapse keeps one review image per element.
- verifies compositor screenshots replace blank native canvas exports when they share DOM order.
- verifies nonblank native canvas exports are preferred over duplicate screenshot fallbacks.
- verifies the deep-capture finalization pipeline drops blank frames, merges duplicates across
  capture layers, flags size-outliers, and produces ordered review items.
"""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import types
from unittest import TestCase, main
from unittest.mock import patch

from PIL import Image

PROJECT_ROOT = Path(__file__).resolve().parents[2]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

new_project_package = types.ModuleType("modules.new_project")
new_project_package.__path__ = [str(Path(__file__).resolve().parent)]  # type: ignore[attr-defined]
sys.modules.setdefault("modules.new_project", new_project_package)

browser_f_module = types.ModuleType("modules.browser_f")
browser_f_module.get_origin = lambda _url: ""  # type: ignore[attr-defined]
sys.modules.setdefault("modules.browser_f", browser_f_module)

from modules.new_project.adv_fetch_cloak_cli import (  # noqa: E402
    CloakFetchDaemon,
    DEEP_CAPTURE_INIT_JS,
    DRAIN_DEEP_CAPTURE_JS,
    DOWNLOAD_METHOD_MEMORY,
    DeepCaptureDomOrder,
    _assign_deep_capture_confidence,
    _cluster_deep_records_by_content,
    _collapse_deep_capture_dom_updates,
    _deep_capture_dom_order,
    _append_first_seen_keys,
    _combine_dom_order,
    _deep_capture_dom_keys_from_raw,
    _deep_capture_element_key,
    _deep_capture_is_canvas_source,
    _deep_capture_source_rank,
    _drop_blank_deep_records,
    _format_console_location,
    _image_dhash,
    _image_looks_blank,
    _phash_distance,
    _select_cluster_representative,
)


def _gradient_image(width: int, height: int, *, horizontal: bool = True) -> Image.Image:
    """Build a non-blank gradient image so blank-frame heuristics treat it as content."""
    image = Image.new("RGB", (width, height))
    pixels = image.load()
    for y in range(height):
        for x in range(width):
            base = (x * 255 // max(width - 1, 1)) if horizontal else (y * 255 // max(height - 1, 1))
            pixels[x, y] = (base, (base * 2) % 256, (base + 80) % 256)
    return image


def _phash_record(
    image: Image.Image,
    *,
    source: str,
    source_index: int,
    dom_order: int | None,
) -> dict[str, object]:
    metadata: dict[str, object] = {"element": "canvas"}
    if dom_order is not None:
        metadata["dom_order"] = dom_order
    return {
        "entry": {"source": source, "metadata": metadata},
        "image": image,
        "source_index": source_index,
        "blank": _image_looks_blank(image),
        "phash": _image_dhash(image),
    }


def _record(
    *,
    source: str,
    dom_order: int,
    source_index: int,
    color: tuple[int, int, int],
) -> dict[str, object]:
    image = Image.new("RGB", (4, 5), color)
    return {
        "entry": {
            "source": source,
            "metadata": {"dom_order": dom_order, "element": "canvas"},
        },
        "image": image,
        "source_index": source_index,
        "blank": _image_looks_blank(image),
    }


class _ConsoleMessage:
    def __init__(self, message_type: str, text: str, location: dict[str, object]) -> None:
        self.type = message_type
        self.text = text
        self.location = location


class _ResponseBodyTrap:
    ok = True
    url = "https://example.test/api"
    headers = {"content-type": "application/json"}

    def body(self) -> bytes:
        raise AssertionError("non-image response body should not be read")


class CloakPageDiagnosticTests(TestCase):
    def test_console_location_formats_url_line_and_column(self) -> None:
        self.assertEqual(
            _format_console_location(
                {"url": "https://example.test/chapter", "lineNumber": 12, "columnNumber": 7}
            ),
            "https://example.test/chapter:12:7",
        )

    def test_console_warning_is_forwarded_to_daemon_log(self) -> None:
        daemon = CloakFetchDaemon()
        message = _ConsoleMessage("warning", "asset failed", {"url": "https://example.test", "lineNumber": 3})

        with patch.object(daemon, "_emit_log") as emit_log:
            daemon._emit_page_console_diagnostic(message)

        emit_log.assert_called_once_with(
            "warn",
            "web page console warning: asset failed (https://example.test:3)",
        )

    def test_console_info_is_not_forwarded_to_daemon_log(self) -> None:
        daemon = CloakFetchDaemon()
        message = _ConsoleMessage("log", "normal trace", {})

        with patch.object(daemon, "_emit_log") as emit_log:
            daemon._emit_page_console_diagnostic(message)

        emit_log.assert_not_called()

    def test_response_objects_are_not_queued_when_deep_capture_is_inactive(self) -> None:
        daemon = CloakFetchDaemon()

        daemon._remember_response_body(object())

        self.assertEqual(daemon._pending_responses, [])

    def test_response_memory_method_is_skipped_without_cached_bodies(self) -> None:
        daemon = CloakFetchDaemon()

        methods = daemon._download_method_order("https://example.test/page.png")

        self.assertNotIn(DOWNLOAD_METHOD_MEMORY, methods)

    def test_non_image_response_body_is_not_read_during_deep_capture(self) -> None:
        daemon = CloakFetchDaemon()
        daemon._deep_capture_active = True

        daemon._cache_response_body_on_owner_thread(_ResponseBodyTrap())

        self.assertEqual(daemon._response_bodies, {})

    def test_deep_capture_init_is_observe_only_and_idempotent(self) -> None:
        # Idempotent install guard so the hooks are wrapped at most once per frame.
        self.assertIn("window.__mfDeepCaptureInstalled", DEEP_CAPTURE_INIT_JS)
        # Capture hooks for the DRM/descramble delivery categories are present...
        self.assertIn("URL.createObjectURL", DEEP_CAPTURE_INIT_JS)
        self.assertIn("convertToBlob", DEEP_CAPTURE_INIT_JS)
        self.assertIn("preserveDrawingBuffer", DEEP_CAPTURE_INIT_JS)
        # ...each wrapper calls the native function and spoofs toString (observe-only).
        self.assertIn(".call(this", DEEP_CAPTURE_INIT_JS)
        self.assertIn("wrapper.toString = () => native.toString()", DEEP_CAPTURE_INIT_JS)
        # Network/transport APIs are never replaced inside the page.
        self.assertNotIn("window.fetch =", DEEP_CAPTURE_INIT_JS)
        self.assertNotIn("XMLHttpRequest.prototype", DEEP_CAPTURE_INIT_JS)
        self.assertNotIn("window.Worker =", DEEP_CAPTURE_INIT_JS)

    def _accumulate(self, *scans):
        order: list = []
        seen: set = set()
        for scan in scans:
            _append_first_seen_keys(order, seen, list(scan))
        return order

    def test_first_seen_appends_overlapping_windows(self) -> None:
        # Scrolling windows that overlap stitch together in reading order.
        order = self._accumulate(
            [("image", "p1"), ("image", "p2"), ("image", "p3")],
            [("image", "p2"), ("image", "p3"), ("image", "p4")],
        )
        self.assertEqual(
            order,
            [("image", "p1"), ("image", "p2"), ("image", "p3"), ("image", "p4")],
        )

    def test_first_seen_keeps_order_across_nonoverlapping_windows(self) -> None:
        # The regression case: a fast scroll catches disjoint windows. First-seen append
        # keeps groups in reading order instead of dropping the later group at the front.
        order = self._accumulate(
            [("image", "p1"), ("image", "p2"), ("image", "p3")],
            [("image", "p7"), ("image", "p8"), ("image", "p9")],
        )
        self.assertEqual(
            order,
            [("image", f"p{n}") for n in (1, 2, 3, 7, 8, 9)],
        )

    def test_combine_dom_order_prepends_only_vanished_pages(self) -> None:
        # Pages still on the page at stop keep the live DOM order untouched; pages that
        # scrolled out (here the first ones) are prepended in first-seen order.
        seen = [("image", f"p{n}") for n in (1, 2, 3, 4, 5)]
        stop = [("image", "p4"), ("image", "p5")]
        self.assertEqual(
            _combine_dom_order(seen, stop),
            [("image", f"p{n}") for n in (1, 2, 3, 4, 5)],
        )

    def test_combine_dom_order_trusts_stop_order_for_present_pages(self) -> None:
        # If first-seen order disagrees with the live DOM order for present pages, the
        # stop DOM order wins (it is the reliable order for everything still on the page).
        seen = [("image", "b"), ("image", "a")]
        stop = [("image", "a"), ("image", "b")]
        self.assertEqual(_combine_dom_order(seen, stop), [("image", "a"), ("image", "b")])

    def test_dom_keys_from_raw_preserves_order_and_kinds(self) -> None:
        raw = [
            {"order": 0, "kind": "image", "url": "u0"},
            {"order": 0, "kind": "image", "url": "u0"},  # duplicate variant dropped
            {"order": 1, "kind": "canvas", "element_id": 7},
            {"order": 2, "kind": "image", "url": ""},  # empty url skipped
            {"order": 3, "kind": "canvas", "element_id": 0},  # non-positive skipped
        ]
        self.assertEqual(
            _deep_capture_dom_keys_from_raw(raw),
            [("image", "u0"), ("canvas", "7")],
        )

    def test_latest_opened_prefers_newest_open_order(self) -> None:
        # The active-tab fallback resolves a candidate group to its most recently opened
        # member, so a freshly opened chapter tab wins over the first-opened list tab.
        daemon = CloakFetchDaemon()
        first, second, third = object(), object(), object()
        daemon._page_open_order = [first, second, third]
        self.assertIs(daemon._latest_opened([first, third]), third)
        self.assertIs(daemon._latest_opened([second, first]), second)
        self.assertIsNone(daemon._latest_opened([]))
        # A page not tracked in open order falls back to the last of the given list.
        untracked = object()
        self.assertIs(daemon._latest_opened([untracked]), untracked)

    def test_deep_capture_source_kind_split(self) -> None:
        # The live status splits captures into canvases vs. regular images; only
        # canvas-element captures count as canvases, everything else as images.
        self.assertTrue(_deep_capture_is_canvas_source("canvas-native"))
        self.assertTrue(_deep_capture_is_canvas_source("canvas-screenshot"))
        self.assertFalse(_deep_capture_is_canvas_source("img-element"))
        self.assertFalse(_deep_capture_is_canvas_source("network"))
        self.assertFalse(_deep_capture_is_canvas_source("cdp-network"))
        self.assertFalse(_deep_capture_is_canvas_source("createObjectURL"))
        self.assertFalse(_deep_capture_is_canvas_source("offscreen-convertToBlob"))

    def test_deep_capture_reads_plain_img_tags_live(self) -> None:
        # Plain <img> tags are read straight from the page (so they are counted live,
        # not only when their network response happens to be seen), covering
        # http(s)/blob:/data: sources and carrying the page session for authorized images.
        self.assertIn("state.scanImages", DEEP_CAPTURE_INIT_JS)
        self.assertIn("img-element", DEEP_CAPTURE_INIT_JS)
        self.assertIn('credentials: "include"', DEEP_CAPTURE_INIT_JS)
        self.assertIn("blob:|data:", DEEP_CAPTURE_INIT_JS)
        # Opaque cross-origin responses (no CORS) are skipped; the network layer keeps them.
        self.assertIn('response.type === "opaque"', DEEP_CAPTURE_INIT_JS)
        # The scan is observe-only: it calls native fetch, never replaces it.
        self.assertNotIn("window.fetch =", DEEP_CAPTURE_INIT_JS)
        # Each drain pass kicks off a fresh scan so newly rendered images are picked up.
        self.assertIn("state.scanImages", DRAIN_DEEP_CAPTURE_JS)


class CloakDeepCaptureTests(TestCase):
    def test_dom_order_is_read_from_capture_metadata(self) -> None:
        self.assertEqual(
            _deep_capture_dom_order({"metadata": {"dom_order": "12"}}),
            12,
        )

    def test_element_key_prefers_weakmap_id_over_dom_order(self) -> None:
        entry = {"metadata": {"element_id": 7, "dom_order": 2}}
        self.assertEqual(_deep_capture_element_key(entry), ("element_id", 7))

    def test_element_key_falls_back_to_dom_order(self) -> None:
        entry = {"metadata": {"dom_order": 4}}
        self.assertEqual(_deep_capture_element_key(entry), ("dom_order", 4))

    def test_element_key_is_none_for_non_dom_capture(self) -> None:
        self.assertIsNone(_deep_capture_element_key({"metadata": {}}))

    def test_collapse_uses_weakmap_id_across_recycled_dom_order(self) -> None:
        # Same canvas element (element_id=9) reported at different DOM positions after
        # virtual-scroll recycling must collapse to a single page.
        first = {
            "entry": {"source": "canvas-native", "metadata": {"element_id": 9, "dom_order": 1}},
            "image": Image.new("RGB", (4, 5), (10, 20, 30)),
            "source_index": 1,
            "blank": False,
        }
        latest = {
            "entry": {"source": "canvas-native", "metadata": {"element_id": 9, "dom_order": 6}},
            "image": Image.new("RGB", (4, 5), (20, 30, 40)),
            "source_index": 4,
            "blank": False,
        }

        with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False):
            collapsed = _collapse_deep_capture_dom_updates([first, latest])

        self.assertEqual(collapsed, [latest])

    def test_offscreen_and_blob_sources_outrank_network(self) -> None:
        # Descrambled/decrypted final images must beat raw network payloads.
        self.assertLess(_deep_capture_source_rank("offscreen-convertToBlob"), _deep_capture_source_rank("network"))
        self.assertLess(_deep_capture_source_rank("createObjectURL"), _deep_capture_source_rank("network"))

    def test_dom_updates_keep_latest_frame_for_same_source(self) -> None:
        first = _record(source="canvas-native", dom_order=3, source_index=1, color=(10, 20, 30))
        latest = _record(source="canvas-native", dom_order=3, source_index=5, color=(20, 30, 40))

        with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False):
            collapsed = _collapse_deep_capture_dom_updates([first, latest])

        self.assertEqual(collapsed, [latest])

    def test_screenshot_replaces_blank_native_canvas(self) -> None:
        blank_native = _record(source="canvas-native", dom_order=4, source_index=1, color=(0, 0, 0))
        screenshot = _record(source="canvas-screenshot", dom_order=4, source_index=2, color=(20, 30, 40))

        with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False):
            collapsed = _collapse_deep_capture_dom_updates([blank_native, screenshot])

        self.assertEqual(collapsed, [screenshot])

    def test_nonblank_native_canvas_beats_screenshot_fallback(self) -> None:
        native = _record(source="canvas-native", dom_order=5, source_index=1, color=(20, 30, 40))
        screenshot = _record(source="canvas-screenshot", dom_order=5, source_index=2, color=(20, 30, 40))

        with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False):
            collapsed = _collapse_deep_capture_dom_updates([native, screenshot])

        self.assertEqual(collapsed, [native])


class CloakDeepCaptureFinalizeTests(TestCase):
    def test_dhash_matches_for_same_image_after_resize(self) -> None:
        original = _gradient_image(200, 300)
        resized = original.resize((100, 150))

        distance = _phash_distance(_image_dhash(original), _image_dhash(resized))

        self.assertLessEqual(distance, 5)

    def test_dhash_differs_for_unrelated_content(self) -> None:
        horizontal = _gradient_image(120, 120, horizontal=True)
        vertical = _gradient_image(120, 120, horizontal=False)

        distance = _phash_distance(_image_dhash(horizontal), _image_dhash(vertical))

        self.assertGreater(distance, 5)

    def test_near_black_frame_with_stray_pixels_is_blank(self) -> None:
        image = Image.new("RGB", (200, 200), (0, 0, 0))
        # A handful of compositor/antialiased pixels (0.005%) must not save a black frame.
        for x in range(10):
            image.putpixel((x, 0), (255, 255, 255))

        self.assertTrue(_image_looks_blank(image))

    def test_sparse_content_page_is_not_blank(self) -> None:
        image = Image.new("RGB", (200, 200), (255, 255, 255))
        # A real page keeps well over the 0.1% tolerance as non-white line art.
        for y in range(40):
            for x in range(200):
                image.putpixel((x, y), (0, 0, 0))

        self.assertFalse(_image_looks_blank(image))

    def test_drop_blank_removes_single_colour_frames(self) -> None:
        blank = _phash_record(Image.new("RGB", (50, 50), (0, 0, 0)), source="canvas-native", source_index=0, dom_order=0)
        content = _phash_record(_gradient_image(50, 50), source="canvas-native", source_index=1, dom_order=1)

        retained = _drop_blank_deep_records([blank, content])

        self.assertEqual(retained, [content])

    def test_cluster_merges_same_page_across_layers(self) -> None:
        page = _gradient_image(200, 300)
        canvas = _phash_record(page, source="canvas-native", source_index=0, dom_order=0)
        network = _phash_record(page.resize((100, 150)), source="network", source_index=1, dom_order=None)
        other = _phash_record(_gradient_image(200, 300, horizontal=False), source="canvas-native", source_index=2, dom_order=1)

        clusters = _cluster_deep_records_by_content([canvas, network, other])

        self.assertEqual(len(clusters), 2)
        merged = next(cluster for cluster in clusters if len(cluster) == 2)
        # The canvas-native record wins as the higher-fidelity representative.
        self.assertEqual(_select_cluster_representative(merged), canvas)

    def test_assign_confidence_flags_small_outlier(self) -> None:
        page = _phash_record(_gradient_image(200, 300), source="canvas-native", source_index=0, dom_order=0)
        icon = _phash_record(_gradient_image(20, 20), source="canvas-native", source_index=1, dom_order=1)

        _assign_deep_capture_confidence([page, icon])

        self.assertFalse(page["probable_junk"])
        self.assertTrue(icon["probable_junk"])

    def test_build_pipeline_drops_blanks_merges_orders_and_flags(self) -> None:
        daemon = CloakFetchDaemon()
        with tempfile.TemporaryDirectory() as tmp:
            output_dir = Path(tmp) / "out"
            raw_dir = output_dir / "_raw"
            raw_dir.mkdir(parents=True)

            page = _gradient_image(200, 300)
            entries = [
                self._write_entry(raw_dir, page, name="page_canvas.png", source="canvas-native", dom_order=0),
                self._write_entry(raw_dir, Image.new("RGB", (200, 300), (0, 0, 0)), name="blank.png", source="canvas-native", dom_order=1),
                self._write_entry(raw_dir, page.resize((100, 150)), name="page_net.png", source="network", dom_order=None),
                self._write_entry(raw_dir, _gradient_image(20, 20, horizontal=False), name="icon.png", source="canvas-native", dom_order=2),
            ]

            with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False), \
                    patch.object(daemon, "_emit_progress"):
                result = daemon._build_auto_result_from_deep_entries(
                    entries, "https://example.test/chapter", output_dir, None
                )

        items = result["items"]
        # Blank dropped; page+network merged into one; icon kept but flagged. Two pages total.
        self.assertEqual(result["downloaded_images"], 2)
        self.assertEqual([item["order"] for item in items], [0, 1])
        self.assertFalse(items[0]["probable_junk"])
        self.assertTrue(items[1]["probable_junk"])
        self.assertEqual(items[0]["width"], 200)
        self.assertEqual(items[0]["height"], 300)

    def test_network_pages_follow_dom_document_order(self) -> None:
        daemon = CloakFetchDaemon()
        with tempfile.TemporaryDirectory() as tmp:
            output_dir = Path(tmp) / "out"
            raw_dir = output_dir / "_raw"
            raw_dir.mkdir(parents=True)

            # Distinct non-blank images of slightly different heights so the output order
            # is identifiable; URL-embedded numbers deliberately disagree with reading order.
            first = self._write_entry(raw_dir, _gradient_image(200, 300, horizontal=True), name="a.png", source="network", dom_order=None)
            second = self._write_entry(raw_dir, _gradient_image(200, 310, horizontal=False), name="b.png", source="network", dom_order=None)
            third = self._write_entry(raw_dir, _gradient_image(200, 320, horizontal=True).transpose(Image.FLIP_LEFT_RIGHT), name="c.png", source="network", dom_order=None)
            first["url"] = "https://mangalib.test/img/page-300.jpg"
            second["url"] = "https://mangalib.test/img/page-100.jpg"
            third["url"] = "https://mangalib.test/img/page-200.jpg"
            dom_order = DeepCaptureDomOrder(
                url_to_index={first["url"]: 0, second["url"]: 1, third["url"]: 2},
                element_to_index={},
            )

            with patch("modules.new_project.adv_fetch_cloak_cli.VERBOSE_DOWNLOAD_LOG", False), \
                    patch.object(daemon, "_emit_progress"):
                result = daemon._build_auto_result_from_deep_entries(
                    [first, second, third], "https://mangalib.test/ch", output_dir, None, dom_order
                )

        # By URL numbers alone the order would be 100,200,300 -> heights 310,320,300.
        # DOM document order must instead yield the appearance order -> 300,310,320.
        self.assertEqual([item["height"] for item in result["items"]], [300, 310, 320])

    @staticmethod
    def _write_entry(
        raw_dir: Path,
        image: Image.Image,
        *,
        name: str,
        source: str,
        dom_order: int | None,
    ) -> dict[str, object]:
        raw_path = raw_dir / name
        image.save(raw_path, format="PNG")
        metadata: dict[str, object] = {"element": "canvas"}
        if dom_order is not None:
            metadata["dom_order"] = dom_order
        return {
            "source": source,
            "url": f"https://example.test/{name}",
            "raw_path": str(raw_path),
            "metadata": metadata,
        }


if __name__ == "__main__":
    main()
