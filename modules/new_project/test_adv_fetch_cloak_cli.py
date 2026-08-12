"""
File: modules/new_project/test_adv_fetch_cloak_cli.py

Purpose:
Unit tests for `CloakFetchDaemon`'s active-tab resolution and for the deep-capture
page-ordering pipeline (`modules/new_project/adv_fetch_cloak_cli.py`).

Main responsibilities:
- `_resolve_active_page` picks the tab with the newest activation timestamp from the
  injected monitor, regardless of tab order — no first-tab / tracked-tab / open-order
  bias;
- ties on timestamp are broken by current visibility;
- a single live real-URL tab is used as-is (no monitor read needed);
- a chosen tab that is already closed triggers a live re-resolve;
- `_active_page_url` raises the standard error when the active tab is blank;
- `_combine_dom_order` splices keys that left the DOM back between their first-seen
  neighbours instead of hoisting them into a leading block (the mixed canvas/`<img>`
  virtual-scroll reader case), while keys still present keep their stop-time order;
- `_append_first_seen_keys` keeps non-overlapping scroll windows in reading order;
- the site-published page index travels raw JS payload -> DOM keys ->
  `DeepCaptureDomOrder` -> `_deep_capture_sort_key`;
- that index is accepted (`_site_page_index_authority`) only when it looks like a real
  reading index — counted per DOM element, present in every key space, discriminative,
  and agreeing with document order — and once accepted its gaps are filled so untagged
  records keep their document position;
- `stop_deep_intercept` accumulates the DOM order on both sides of the canvas screenshot
  pass, which scrolls (and therefore recycles) elements.

Notes:
Fake pages record `evaluate`/`bring_to_front` and never touch a real browser. `_valid_pages`
is patched to return them, bypassing `_ensure_browser`. Ordering tests patch
`_read_dom_order_keys` instead, so no browser or network is involved anywhere. The tests
avoid pytest fixtures and expose a `__main__` runner so they pass under both `pytest` and a
plain `python3` invocation.
"""

from __future__ import annotations

import threading
from pathlib import Path
from typing import Any, Optional

from PIL import Image

from modules.new_project.adv_fetch_cloak_cli import (
    CloakFetchDaemon,
    DeepCaptureDomOrder,
    DeepCaptureState,
    _append_first_seen_keys,
    _combine_dom_order,
    _deep_capture_dom_keys_from_raw,
    _deep_capture_sort_key,
    _site_page_index_authority,
)


class _FakePage:
    """Minimal Playwright-page stand-in for active-tab resolution tests."""

    def __init__(self, url: str, active_ts: float, visible: bool = False, closed: bool = False) -> None:
        self.url = url
        self._active_ts = active_ts
        self._visible = visible
        self._closed = closed
        self.brought_to_front = 0

    def is_closed(self) -> bool:
        return self._closed

    def evaluate(self, _js: str) -> Any:
        # Only ACTIVE_MONITOR_READ_JS is evaluated across candidate tabs.
        return {"a": self._active_ts, "vis": self._visible}

    def bring_to_front(self) -> None:
        self.brought_to_front += 1

    def on(self, *_args: Any, **_kwargs: Any) -> None:
        pass


def _daemon_with_pages(page_batches: list[list[_FakePage]]) -> CloakFetchDaemon:
    """Build a daemon whose `_valid_pages` yields successive batches per call."""
    daemon = CloakFetchDaemon()
    calls = {"i": 0}

    def fake_valid_pages() -> list[Any]:
        idx = min(calls["i"], len(page_batches) - 1)
        calls["i"] += 1
        return list(page_batches[idx])

    daemon._valid_pages = fake_valid_pages  # type: ignore[assignment]
    return daemon


def test_picks_newest_activation_regardless_of_order() -> None:
    a = _FakePage("https://site/a", active_ts=100.0)
    b = _FakePage("https://site/b", active_ts=300.0)  # most recently activated
    c = _FakePage("https://site/c", active_ts=200.0)
    daemon = _daemon_with_pages([[a, c, b]])  # order deliberately not by timestamp

    chosen = daemon._resolve_active_page("test")

    assert chosen is b
    assert daemon._page is b
    assert b.brought_to_front == 1


def test_visibility_breaks_timestamp_tie() -> None:
    a = _FakePage("https://site/a", active_ts=0.0, visible=False)
    b = _FakePage("https://site/b", active_ts=0.0, visible=True)  # tie on ts, but visible
    daemon = _daemon_with_pages([[a, b]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is b


def test_single_tab_used_as_is() -> None:
    only = _FakePage("https://site/only", active_ts=0.0)
    daemon = _daemon_with_pages([[only]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is only


def test_closed_chosen_tab_triggers_reresolve() -> None:
    dead = _FakePage("https://site/dead", active_ts=999.0, closed=True)
    live = _FakePage("https://site/live", active_ts=10.0)
    # First batch: only the (single) dead tab -> chosen but is_closed -> re-resolve.
    # Second batch: a live tab -> returned.
    daemon = _daemon_with_pages([[dead], [live]])

    chosen = daemon._resolve_active_page("test")

    assert chosen is live


def test_active_page_url_rejects_blank() -> None:
    # A resolver that yields a blank-URL page (filtered out -> no valid -> _require_page).
    daemon = CloakFetchDaemon()
    blank = _FakePage("about:blank", active_ts=0.0)
    daemon._resolve_active_page = lambda reason: blank  # type: ignore[assignment]

    raised: Optional[Exception] = None
    try:
        daemon._active_page_url("test")
    except RuntimeError as exc:
        raised = exc

    assert raised is not None
    assert "CloakBrowser" in str(raised)


# --------------------------------------------------------------------------------------
# Deep-capture page ordering
#
# Fixtures model the real comix.to shape that exposed the bug: a 99-page chapter whose
# containers all carry `data-page="N"` (1-based), rendered as <img> except pages
# 10,20,...,90 which are <canvas>. The <img> elements persist in the DOM once rendered;
# the canvases are torn down when scrolled away, so they are absent at stop.
#
# That container shape is not speculation: it was verified against the live chapter page
# (https://comix.to/title/keqvv-whos-that-girl/11104613-chapter-71), where all 99
# `.rpage-page` containers carry a 1-based `data-page` and all of them exist from first
# paint. The canvas/<img> split below is the fixture's own simplification of the mixed
# rendering, so the ordering pipeline is exercised on both key spaces at once.
# --------------------------------------------------------------------------------------

PAGE_COUNT = 99
CANVAS_PAGES = tuple(range(10, PAGE_COUNT + 1, 10))  # 10, 20, ... 90


def _is_canvas_page(page: int) -> bool:
    return page in CANVAS_PAGES


def _img_url(page: int) -> str:
    return f"https://cdn.example/chapter/{page:04}.webp"


def _img_key(page: int) -> tuple[str, str]:
    return ("image", _img_url(page))


def _canvas_key(page: int) -> tuple[str, str]:
    # Canvas WeakMap ids are arbitrary but stable; use the page number so the fixture
    # stays readable. They live in a different key space than image URLs.
    return ("canvas", str(page))


def _site_keys(pages: list[int]) -> list[tuple[str, str]]:
    """DOM keys for `pages`, in reading order, with the site's canvas/img split."""
    return [_canvas_key(page) if _is_canvas_page(page) else _img_key(page) for page in pages]


def _img_variant_urls(page: int, variants: int) -> list[str]:
    """The URL variants one `<img>` emits (currentSrc/src/attribute src/data-src)."""
    base = _img_url(page)
    return [base] + [f"{base}?v={n}" for n in range(1, variants)]


def _raw_payload(
    pages: list[int],
    *,
    with_page_index: bool = True,
    index_canvases: bool = True,
    url_variants: int = 1,
) -> list[dict[str, Any]]:
    """Build a COLLECT_DOM_IMAGE_ORDER_JS-shaped payload for `pages`, in reading order.

    `url_variants` emits that many URLs per `<img>` sharing one `order` slot, the shape
    the real collector produces; `index_canvases=False` models a reader that tags only
    its `<img>` containers.
    """
    payload: list[dict[str, Any]] = []
    for order, page in enumerate(pages):
        if _is_canvas_page(page):
            item: dict[str, Any] = {"order": order, "kind": "canvas", "element_id": page}
            if with_page_index and index_canvases:
                item["page_index"] = page
            payload.append(item)
            continue
        for url in _img_variant_urls(page, url_variants):
            item = {"order": order, "kind": "image", "url": url}
            if with_page_index:
                item["page_index"] = page
            payload.append(item)
    return payload


def _deep_capture_daemon() -> tuple[CloakFetchDaemon, DeepCaptureState]:
    """A daemon with an active, browser-free deep-capture state ready for order tests."""
    daemon = CloakFetchDaemon()
    capture = DeepCaptureState(
        stop_event=threading.Event(),
        lock=threading.Lock(),
        entries=[],
        hashes=set(),
        page_url="https://example/chapter",
        output_dir=Path("."),
        raw_dir=Path("."),
    )
    daemon._deep_capture = capture
    daemon._deep_capture_active = True
    return daemon, capture


def test_combine_dom_order_interleaves_vanished_canvases() -> None:
    # Regression test: canvases vanish, imgs persist -> the canvases must stay between
    # their neighbouring imgs, not be hoisted into a leading block.
    all_pages = list(range(1, PAGE_COUNT + 1))
    seen_keys = _site_keys(all_pages)
    stop_keys = _site_keys([page for page in all_pages if not _is_canvas_page(page)])

    combined = _combine_dom_order(seen_keys, stop_keys)

    assert combined == seen_keys
    # Explicitly: the first canvas sits between page 9 and page 11, not at position 0.
    assert combined.index(_canvas_key(10)) == 9
    assert combined[8] == _img_key(9)
    assert combined[10] == _img_key(11)


def test_combine_dom_order_no_vanished_keys_is_stop_order() -> None:
    # Degenerate case: everything still present -> identical to a single stop-time read,
    # even when first-seen accumulation disagrees with it.
    seen_keys = _site_keys([3, 1, 2])
    stop_keys = _site_keys([1, 2, 3])

    assert _combine_dom_order(seen_keys, stop_keys) == stop_keys


def test_combine_dom_order_everything_vanished_is_first_seen_order() -> None:
    seen_keys = _site_keys([1, 2, 3])

    assert _combine_dom_order(seen_keys, []) == seen_keys


def test_combine_dom_order_empty_inputs() -> None:
    assert _combine_dom_order([], []) == []


def test_combine_dom_order_trailing_and_leading_vanished() -> None:
    # Leading vanished keys (early pages recycled out of a virtual-scroll reader) still
    # land in front; trailing ones with no surviving successor land at the end.
    seen_keys = _site_keys([1, 2, 3, 4, 5])
    stop_keys = _site_keys([3])

    assert _combine_dom_order(seen_keys, stop_keys) == _site_keys([1, 2, 3, 4, 5])


def test_combine_dom_order_keeps_stop_only_keys() -> None:
    # Keys that appear for the first time at stop are kept in their stop position.
    seen_keys = _site_keys([2])
    stop_keys = _site_keys([1, 2, 3])

    assert _combine_dom_order(seen_keys, stop_keys) == stop_keys


def test_append_first_seen_keys_non_overlapping_windows() -> None:
    # A fast scroll yields disjoint windows; each is appended after the previous one.
    order: list[tuple[str, str]] = []
    seen: set[tuple[str, str]] = set()

    _append_first_seen_keys(order, seen, _site_keys([1, 2, 3]))
    _append_first_seen_keys(order, seen, _site_keys([11, 12]))
    _append_first_seen_keys(order, seen, _site_keys([21]))

    assert order == _site_keys([1, 2, 3, 11, 12, 21])


def test_append_first_seen_keys_ignores_repeats() -> None:
    order: list[tuple[str, str]] = []
    seen: set[tuple[str, str]] = set()

    _append_first_seen_keys(order, seen, _site_keys([1, 2, 3]))
    _append_first_seen_keys(order, seen, _site_keys([2, 3, 4]))

    assert order == _site_keys([1, 2, 3, 4])


def test_dom_keys_from_raw_reads_page_index() -> None:
    reading = _deep_capture_dom_keys_from_raw(_raw_payload([9, 10, 11]))

    assert reading.keys == _site_keys([9, 10, 11])
    assert reading.page_indices == {_img_key(9): 9, _canvas_key(10): 10, _img_key(11): 11}
    # One key per element here, so every key is its own element representative.
    assert reading.key_elements == {key: key for key in reading.keys}


def test_dom_keys_from_raw_rejects_non_integer_page_index() -> None:
    raw = [
        {"order": 0, "kind": "image", "url": _img_url(1), "page_index": "2"},
        {"order": 1, "kind": "image", "url": _img_url(2), "page_index": -1},
        {"order": 2, "kind": "image", "url": _img_url(3), "page_index": 1.5},
        {"order": 3, "kind": "image", "url": _img_url(4), "page_index": True},
        {"order": 4, "kind": "image", "url": _img_url(5)},
    ]

    reading = _deep_capture_dom_keys_from_raw(raw)

    assert reading.keys == _site_keys([1, 2, 3, 4, 5])
    assert reading.page_indices == {}


def test_dom_keys_from_raw_groups_url_variants_into_one_element() -> None:
    # The real collector emits up to four URLs per <img>, all sharing one `order` slot;
    # they must collapse to one element so coverage ratios are not skewed by them.
    reading = _deep_capture_dom_keys_from_raw(_raw_payload([1], url_variants=4))

    assert len(reading.keys) == 4
    assert set(reading.key_elements.values()) == {_img_key(1)}


def test_dom_keys_from_raw_treats_slotless_items_as_own_elements() -> None:
    # A payload without a usable `order` must not merge unrelated keys into one element.
    raw = [
        {"kind": "image", "url": _img_url(1)},
        {"order": True, "kind": "image", "url": _img_url(2)},
    ]

    reading = _deep_capture_dom_keys_from_raw(raw)

    assert reading.key_elements == {_img_key(1): _img_key(1), _img_key(2): _img_key(2)}


def _capture_entry(page: int) -> dict[str, Any]:
    """A decoded deep-capture entry as it would arrive for `page`."""
    if _is_canvas_page(page):
        return {
            "source": "canvas-native",
            "url": f"deep://canvas/{page}",
            "metadata": {"element_id": page},
        }
    return {"source": "network", "url": _img_url(page), "metadata": {}}


def _finalize_from_raw(
    full_raw: list[dict[str, Any]], stop_raw: list[dict[str, Any]]
) -> DeepCaptureDomOrder:
    """Run the real accumulate -> stop-read -> finalize chain on two raw payloads."""
    daemon, capture = _deep_capture_daemon()
    daemon._read_dom_order_keys = lambda: _deep_capture_dom_keys_from_raw(full_raw)  # type: ignore[assignment]
    daemon._accumulate_deep_dom_order()
    daemon._read_dom_order_keys = lambda: _deep_capture_dom_keys_from_raw(stop_raw)  # type: ignore[assignment]
    return daemon._finalize_dom_capture_order(capture)


def _finalized_dom_order(
    *,
    with_page_index: bool,
    index_canvases: bool = True,
    url_variants: int = 1,
) -> DeepCaptureDomOrder:
    """Run the finalize chain on the site fixture (canvases vanish before stop)."""
    all_pages = list(range(1, PAGE_COUNT + 1))
    surviving = [page for page in all_pages if not _is_canvas_page(page)]
    return _finalize_from_raw(
        _raw_payload(
            all_pages,
            with_page_index=with_page_index,
            index_canvases=index_canvases,
            url_variants=url_variants,
        ),
        _raw_payload(
            surviving,
            with_page_index=with_page_index,
            index_canvases=index_canvases,
            url_variants=url_variants,
        ),
    )


def _sorted_pages(dom_order: Optional[DeepCaptureDomOrder], pages: list[int]) -> list[int]:
    """Sort `pages` the way the deep-capture pipeline would, and report the order."""
    image = Image.new("RGB", (4, 4))
    decorated = [
        (page, _deep_capture_sort_key(_capture_entry(page), image, index, dom_order))
        for index, page in enumerate(pages)
    ]
    decorated.sort(key=lambda pair: pair[1])
    return [page for page, _ in decorated]


def _sorted_urls(dom_order: Optional[DeepCaptureDomOrder], urls: list[str]) -> list[str]:
    """Sort plain image captures identified by URL, reporting the resulting order."""
    image = Image.new("RGB", (4, 4))
    decorated = [
        (
            url,
            _deep_capture_sort_key(
                {"source": "network", "url": url, "metadata": {}}, image, index, dom_order
            ),
        )
        for index, url in enumerate(urls)
    ]
    decorated.sort(key=lambda pair: pair[1])
    return [url for url, _ in decorated]


def _scrambled_capture_order() -> list[int]:
    """Capture order as the real run produced it: every canvas arrived before the imgs."""
    return list(CANVAS_PAGES) + [p for p in range(1, PAGE_COUNT + 1) if not _is_canvas_page(p)]


def test_page_index_path_orders_canvas_and_img_as_one_sequence() -> None:
    dom_order = _finalized_dom_order(with_page_index=True)

    assert dom_order.authoritative is True
    assert dom_order.url_to_page[_img_url(11)] == 11
    assert dom_order.element_to_page[10] == 10

    assert _sorted_pages(dom_order, _scrambled_capture_order()) == list(range(1, PAGE_COUNT + 1))


def test_page_index_survives_multiple_url_variants_per_image() -> None:
    # The real collector emits up to four URLs per <img>; counting keys instead of
    # elements would let the images outvote the canvases. All four variants must resolve
    # to the same page slot.
    dom_order = _finalized_dom_order(with_page_index=True, url_variants=4)

    assert dom_order.authoritative is True
    for url in _img_variant_urls(11, 4):
        assert dom_order.url_to_page[url] == 11
    assert dom_order.element_to_page[10] == 10

    assert _sorted_pages(dom_order, _scrambled_capture_order()) == list(range(1, PAGE_COUNT + 1))


def test_page_index_absent_falls_back_to_document_order() -> None:
    # Same site shape without the published index: the combined document order (which
    # the merge now keeps interleaved) must still produce the right sequence.
    dom_order = _finalized_dom_order(with_page_index=False)

    assert dom_order.authoritative is False
    assert dom_order.url_to_page == {}
    assert dom_order.element_to_page == {}

    assert _sorted_pages(dom_order, _scrambled_capture_order()) == list(range(1, PAGE_COUNT + 1))


def test_index_covering_only_the_image_space_is_refused() -> None:
    # A mixed reader that tags only its <img> containers: 90 indexed images (two URL
    # variants each) against 9 untagged canvases. Counting keys made this a landslide
    # "authoritative" and pushed every canvas into a lower tier, which concatenated all
    # nine after the images. The canvas key space has no index, so the gate must refuse
    # and document order must carry the chapter.
    dom_order = _finalized_dom_order(with_page_index=True, index_canvases=False, url_variants=2)

    assert dom_order.authoritative is False
    assert dom_order.url_to_page == {}

    assert _sorted_pages(dom_order, _scrambled_capture_order()) == list(range(1, PAGE_COUNT + 1))


def test_shared_wrapper_index_is_refused_and_document_order_wins() -> None:
    # One wrapper carrying `data-index` gives every page the same value; the resolved
    # indices are then not discriminative at all. Reported symptom of trusting them:
    # [10, 20, ... 90, 1, 2, 3, ...] — the raw capture order.
    all_pages = list(range(1, PAGE_COUNT + 1))
    surviving = [page for page in all_pages if not _is_canvas_page(page)]
    full_raw = _raw_payload(all_pages)
    stop_raw = _raw_payload(surviving)
    for item in (*full_raw, *stop_raw):
        item["page_index"] = 0

    dom_order = _finalize_from_raw(full_raw, stop_raw)

    assert dom_order.authoritative is False
    assert _sorted_pages(dom_order, _scrambled_capture_order()) == list(range(1, PAGE_COUNT + 1))


def test_identical_index_values_tie_break_on_document_order() -> None:
    # Belt and braces for the case above: even if a degenerate index were ever accepted,
    # a tie on the published value must degrade to document order, never to capture order.
    dom_order = DeepCaptureDomOrder(
        url_to_index={_img_url(1): 5, _img_url(2): 2},
        element_to_index={},
        url_to_page={_img_url(1): 7, _img_url(2): 7},
        element_to_page={},
        authoritative=True,
    )

    assert _sorted_urls(dom_order, [_img_url(1), _img_url(2)]) == [_img_url(2), _img_url(1)]


def test_untagged_records_keep_their_document_position() -> None:
    # An authoritative document that still contains untagged elements (a banner between
    # pages 5 and 6). The banner must stay between them instead of being pushed behind
    # every indexed page by a tier change.
    banner = "https://cdn.example/banner.png"
    pages = list(range(1, 21))
    raw: list[dict[str, Any]] = []
    for order, page in enumerate(pages):
        raw.append({"order": order, "kind": "image", "url": _img_url(page), "page_index": page})
        if page == 5:
            raw.append({"order": 1000, "kind": "image", "url": banner})

    dom_order = _finalize_from_raw(raw, raw)

    assert dom_order.authoritative is True
    assert dom_order.url_to_page[banner] == 5
    urls = [_img_url(page) for page in pages] + [banner]

    assert _sorted_urls(dom_order, urls) == [
        *[_img_url(page) for page in range(1, 6)],
        banner,
        *[_img_url(page) for page in range(6, 21)],
    ]


def test_two_colliding_index_sequences_are_refused() -> None:
    # Pages tagged `data-page` 1..20 followed by an ad rail tagged `data-index` 1..20:
    # trusting that would interleave junk as page1, ad1, page2, ad2, ...
    raw: list[dict[str, Any]] = []
    for order, page in enumerate(range(1, 21)):
        raw.append({"order": order, "kind": "image", "url": _img_url(page), "page_index": page})
    for order, page in enumerate(range(1, 21)):
        raw.append(
            {"order": 100 + order, "kind": "image", "url": f"https://ads.example/{page}.png", "page_index": page}
        )

    dom_order = _finalize_from_raw(raw, raw)

    assert dom_order.authoritative is False


def test_indexed_thumbnail_strip_cannot_outvote_the_pages() -> None:
    # A 30-thumbnail chapter strip (each thumbnail an <img> with `data-index`) placed
    # before 20 tagged pages: its numbering restarts, so document order must stay in
    # charge instead of the strip hoisting itself into the page sequence.
    raw: list[dict[str, Any]] = []
    for order, thumb in enumerate(range(30)):
        raw.append(
            {"order": order, "kind": "image", "url": f"https://cdn.example/thumb/{thumb}.png", "page_index": thumb}
        )
    for order, page in enumerate(range(1, 21)):
        raw.append({"order": 100 + order, "kind": "image", "url": _img_url(page), "page_index": page})

    dom_order = _finalize_from_raw(raw, raw)

    assert dom_order.authoritative is False


def test_page_index_authority_gate_boundary() -> None:
    # Coverage is measured per element and must be a strict majority.
    half = [("image", 1), ("image", 2), ("image", None), ("image", None)]
    majority = [("image", 1), ("image", 2), ("image", 3), ("image", None), ("image", None)]

    assert _site_page_index_authority(half)[0] is False
    assert _site_page_index_authority(majority)[0] is True


def test_page_index_authority_rejects_degenerate_inputs() -> None:
    assert _site_page_index_authority([])[0] is False
    # A single indexed element defines no order.
    assert _site_page_index_authority([("image", 4), ("image", None)])[0] is False
    # Every element sharing one value is not an index.
    assert _site_page_index_authority([("image", 3)] * 5)[0] is False
    # A populated key space with no indexed element at all.
    assert (
        _site_page_index_authority([("image", 1), ("image", 2), ("image", 3), ("canvas", None)])[0]
        is False
    )
    # Two elements per value (a <picture> source + img, or a canvas over an img) is a
    # shape a real index can have, and must still be accepted.
    assert (
        _site_page_index_authority(
            [("image", 1), ("canvas", 1), ("image", 2), ("canvas", 2), ("image", 3), ("canvas", 3)]
        )[0]
        is True
    )


def test_sort_key_tier_selection_with_authoritative_index() -> None:
    dom_order = DeepCaptureDomOrder(
        url_to_index={_img_url(1): 7},
        element_to_index={10: 3},
        url_to_page={_img_url(1): 42},
        element_to_page={10: 41},
        authoritative=True,
    )
    image = Image.new("RGB", (4, 4))

    img_key = _deep_capture_sort_key(_capture_entry(1), image, 0, dom_order)
    canvas_key = _deep_capture_sort_key(_capture_entry(10), image, 1, dom_order)

    assert img_key[:2] == (0, 42)
    assert canvas_key[:2] == (0, 41)


def test_sort_key_tier_selection_without_authoritative_index() -> None:
    # Same maps, but not marked authoritative -> the page index must be ignored and the
    # document-order tier used instead.
    dom_order = DeepCaptureDomOrder(
        url_to_index={_img_url(1): 7},
        element_to_index={10: 3},
        url_to_page={_img_url(1): 42},
        element_to_page={10: 41},
        authoritative=False,
    )
    image = Image.new("RGB", (4, 4))

    img_key = _deep_capture_sort_key(_capture_entry(1), image, 0, dom_order)
    canvas_key = _deep_capture_sort_key(_capture_entry(10), image, 1, dom_order)

    assert img_key[:2] == (1, 7)
    assert canvas_key[:2] == (1, 3)


def test_sort_key_without_dom_order_uses_weaker_tiers() -> None:
    image = Image.new("RGB", (4, 4))
    entry = {"source": "network", "url": "https://cdn.example/x.webp", "metadata": {"dom_order": 5}}

    key = _deep_capture_sort_key(entry, image, 0, None)

    assert key[:2] == (2, 5)


def test_page_index_minority_is_not_authoritative() -> None:
    # Only a couple of unrelated elements publish an index -> document order must win.
    raw = _raw_payload([1, 2, 3, 4, 5], with_page_index=False)
    raw[0]["page_index"] = 1
    raw[1]["page_index"] = 2

    dom_order = _finalize_from_raw(raw, raw)

    assert dom_order.authoritative is False


def test_stop_accumulates_dom_order_around_the_screenshot_pass() -> None:
    # `ElementHandle.screenshot()` scrolls elements into view, which recycles them on a
    # virtual-scroll reader. The accumulation taken before that pass is the only surviving
    # evidence for the recycled keys, so it must not be the last one taken.
    daemon, capture = _deep_capture_daemon()
    state = {"raw": _raw_payload([9, 10, 11])}
    recorded: dict[str, Any] = {}

    daemon._read_dom_order_keys = lambda: _deep_capture_dom_keys_from_raw(state["raw"])  # type: ignore[assignment]
    daemon._emit_progress = lambda *_args, **_kwargs: None  # type: ignore[assignment]
    daemon._capture_deep_updates_once = lambda: None  # type: ignore[assignment]
    daemon._settle_deep_image_reads = lambda: None  # type: ignore[assignment]
    daemon._current_url_or = lambda default: default  # type: ignore[assignment]
    daemon._clear_deep_capture_runtime = lambda: None  # type: ignore[assignment]

    def fake_screenshots(_capture: DeepCaptureState) -> None:
        # Scrolling to the last page tore down everything above it.
        state["raw"] = _raw_payload([11])

    def fake_build(
        _entries: Any, _page_url: str, _output_dir: Any, _cancel_file: Any, dom_order: Any
    ) -> dict[str, Any]:
        recorded["dom_order"] = dom_order
        return {}

    daemon._capture_visible_canvas_screenshots_if_needed = fake_screenshots  # type: ignore[assignment]
    daemon._build_auto_result_from_deep_entries = fake_build  # type: ignore[assignment]

    daemon.stop_deep_intercept()

    dom_order = recorded["dom_order"]
    assert dom_order.url_to_index[_img_url(9)] == 0
    assert dom_order.element_to_index[10] == 1
    assert dom_order.url_to_index[_img_url(11)] == 2


if __name__ == "__main__":
    test_picks_newest_activation_regardless_of_order()
    test_visibility_breaks_timestamp_tie()
    test_single_tab_used_as_is()
    test_closed_chosen_tab_triggers_reresolve()
    test_active_page_url_rejects_blank()
    test_combine_dom_order_interleaves_vanished_canvases()
    test_combine_dom_order_no_vanished_keys_is_stop_order()
    test_combine_dom_order_everything_vanished_is_first_seen_order()
    test_combine_dom_order_empty_inputs()
    test_combine_dom_order_trailing_and_leading_vanished()
    test_combine_dom_order_keeps_stop_only_keys()
    test_append_first_seen_keys_non_overlapping_windows()
    test_append_first_seen_keys_ignores_repeats()
    test_dom_keys_from_raw_reads_page_index()
    test_dom_keys_from_raw_rejects_non_integer_page_index()
    test_dom_keys_from_raw_groups_url_variants_into_one_element()
    test_dom_keys_from_raw_treats_slotless_items_as_own_elements()
    test_page_index_path_orders_canvas_and_img_as_one_sequence()
    test_page_index_survives_multiple_url_variants_per_image()
    test_page_index_absent_falls_back_to_document_order()
    test_index_covering_only_the_image_space_is_refused()
    test_shared_wrapper_index_is_refused_and_document_order_wins()
    test_identical_index_values_tie_break_on_document_order()
    test_untagged_records_keep_their_document_position()
    test_two_colliding_index_sequences_are_refused()
    test_indexed_thumbnail_strip_cannot_outvote_the_pages()
    test_page_index_authority_gate_boundary()
    test_page_index_authority_rejects_degenerate_inputs()
    test_sort_key_tier_selection_with_authoritative_index()
    test_sort_key_tier_selection_without_authoritative_index()
    test_sort_key_without_dom_order_uses_weaker_tiers()
    test_page_index_minority_is_not_authoritative()
    test_stop_accumulates_dom_order_around_the_screenshot_pass()
    print("all active-tab resolution and deep-capture ordering tests passed")
