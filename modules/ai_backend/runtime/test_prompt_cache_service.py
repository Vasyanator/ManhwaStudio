"""
File: modules/ai_backend/runtime/test_prompt_cache_service.py

Purpose:
Unit tests of the model-agnostic prompt-cache orchestration
(`prompt_cache_service.py`) over a FAKE model adapter and a real library in a
temp directory: the memory LRU (count and byte caps, copy-on-write index),
`tensor_bytes`, `prompt_library.same_encoder`, the plan (memory vs library,
the width filter, a request without a library), `.status`, serving (a library
read remembered, a vanished file = gone), the build (encode, reuse of memory,
already present, rename), the listing, export, import (own v2, foreign v2,
an unknown marker), the wire-field reader and the refusals' owner prefix.

Notes:
- Torch-free: the tensor (de)serialization of `prompt_library` is stubbed.
- FLUX.2's own behaviour over the same code is pinned by
  `inpaint/flux2_klein/test_prompt_cache.py` and its golden.
"""

from __future__ import annotations

import json
import struct
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.runtime import prompt_cache_service as pcs
from modules.ai_backend.runtime import prompt_library as pl
from modules.ai_backend.runtime.safetensors_stream import read_safetensors_header

LIB = "fake_model"
OTHER_LIB = "fake_model_xl"
ORDER = ("bf16", "q8_0", "fp8")


class _FakeTensor:
    """A tensor stand-in: a shape, the bytes it serializes to (2 per element) and `nbytes`."""

    def __init__(self, shape: tuple[int, ...] = (1, 4, 8), fill: int = 7) -> None:
        self.shape = tuple(shape)
        count = 1
        for dim in self.shape:
            count *= dim
        self.data = bytes([fill]) * (2 * count)
        self.nbytes = len(self.data)


def _container(metadata: dict[str, str], tensor: _FakeTensor, *, name: str = "embeds") -> bytes:
    header = {name: {"dtype": "BF16", "shape": list(tensor.shape), "data_offsets": [0, len(tensor.data)]}, "__metadata__": dict(metadata)}
    blob = json.dumps(header).encode("utf-8")
    blob += b" " * (-(8 + len(blob)) % 8)
    return struct.pack("<Q", len(blob)) + blob + tensor.data


def _fake_serialize(tensor_name: str, tensor: _FakeTensor, metadata: dict[str, str]) -> bytes:
    return _container(dict(metadata), tensor, name=tensor_name)


def _fake_load(path: Path, tensor_name: str) -> _FakeTensor:
    return _FakeTensor(tuple(read_safetensors_header(path)[tensor_name]["shape"]))


def _encoder(source: str = "stock", fmt: str = "bf16", fp8: bool = False, encoder_id: str = "a" * 64) -> pl.EncoderProvenance:
    return pl.EncoderProvenance(source, fmt, fp8, encoder_id, f"{source}-{fmt}")


def _spec(library_id: str) -> pl.LibrarySpec:
    return pl.LibrarySpec(library_id, "embeds", "bfloat16", "BF16")


class _FakeAdapter:
    """A `PromptCacheAdapter` whose request is a plain dict (see `_Case.request`)."""

    owner = "Fake model"

    def __init__(self, root: Path) -> None:
        self.root = root
        self.migrations: list[str | None] = []
        self.legacy: dict[str, Any] | None = None

    def library_of(self, library_id: str) -> pl.PromptLibrary:
        return pl.library_for(self.root, _spec(library_id))

    def accepts_library(self, library_id: str) -> bool:
        return library_id in (LIB, OTHER_LIB)

    def library_id(self, request: dict[str, Any]) -> str | None:
        return request.get("library")

    def texts_needed(self, request: dict[str, Any]) -> list[str]:
        return list(request["texts"])

    def max_sequence_length(self, request: dict[str, Any]) -> int:
        return int(request["msl"])

    def preference(self, request: dict[str, Any]) -> pl.Preference:
        return pl.Preference(request["source"], ORDER)

    def accepted_width(self, request: dict[str, Any]) -> int | None:
        return request["width"]

    def current_provenance(self, request: dict[str, Any]) -> pl.EncoderProvenance | None:
        current = request["current"]
        if current == "unidentifiable":
            raise ValueError("no config.json")
        return current

    def encoder_available(self, request: dict[str, Any]) -> bool:
        return request["current"] is not None

    def migrate(self, request: dict[str, Any]) -> None:
        self.migrations.append(request.get("library"))

    def legacy_report(self) -> dict[str, Any] | None:
        return self.legacy

    def import_foreign(self, header: Any, source: Path, request: dict[str, Any]) -> tuple[str, dict[str, str]]:
        return pcs.refuse_unknown_marker(source)


class _Case(unittest.TestCase):
    """A temp library root, the stub serializer, a fake adapter, an orchestrator with an 8-entry memory."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        for name, replacement in (("serialize_tensor", _fake_serialize), ("load_tensor", _fake_load)):
            attr = patch.object(pl, name, replacement)
            attr.start()
            self.addCleanup(attr.stop)
        self.adapter = _FakeAdapter(self.root / "prompt_cache")
        self.memory = pcs.EmbeddingMemory(8)
        self.orchestrator = pcs.PromptCacheOrchestrator(self.adapter, self.memory)

    def request(self, **overrides: Any) -> dict[str, Any]:
        request: dict[str, Any] = {"library": LIB, "texts": ["a cat"], "msl": 4, "width": 8, "source": "stock", "current": _encoder()}
        request.update(overrides)
        return request

    def add_entry(self, prompt: str = "a cat", *, encoder: pl.EncoderProvenance | None = None, width: int = 8, name: str = "", library_id: str = LIB, created_at: str = "2026-10-01T00:00:00Z") -> pl.LibraryEntry:
        with patch.object(pl, "utc_now", return_value=created_at):
            entry, written = self.adapter.library_of(library_id).write_entry(prompt, _FakeTensor((1, 4, width)), encoder or _encoder(), max_sequence_length=4, name=name, origin=pl.ORIGIN_BUILT)
        self.assertTrue(written)
        return entry

    def remember(self, text: str = "a cat", *, encoder: pl.EncoderProvenance | None = None, library: str = LIB, width: int = 8) -> _FakeTensor:
        tensor = _FakeTensor((1, 4, width))
        self.orchestrator.remember_locked(library, text, 4, encoder or _encoder(), tensor, "2026-10-02T00:00:00Z")
        return tensor


def _remembered(index: int, width: int = 8) -> pcs.MemoryEmbedding:
    encoder = _encoder(encoder_id=f"{index:064d}")
    return pcs.MemoryEmbedding(pcs.memory_key(LIB, f"t{index}", 4, encoder, ""), LIB, f"t{index}", 4, encoder, width, None, "2026-10-01T00:00:00Z")


class EmbeddingMemoryTests(unittest.TestCase):
    """The LRU, its caps and its copy-on-write index."""

    def test_the_count_cap_evicts_the_least_recently_used(self) -> None:
        memory = pcs.EmbeddingMemory(2)
        first, second, third = _remembered(1), _remembered(2), _remembered(3)
        memory.store(first, _FakeTensor())
        memory.store(second, _FakeTensor())
        self.assertIsNotNone(memory.get(first.key), "touching `first` makes `second` the oldest")
        memory.store(third, _FakeTensor())
        self.assertEqual(set(memory.index), {first.key, third.key})
        self.assertIsNone(memory.get(second.key))

    def test_the_byte_cap_evicts_the_oldest_but_never_the_newest(self) -> None:
        memory = pcs.EmbeddingMemory(8, max_bytes=2 * _FakeTensor().nbytes)
        for index in (1, 2, 3):
            memory.store(_remembered(index), _FakeTensor())
        self.assertEqual([item.text for item in memory.index.values()], ["t2", "t3"])
        self.assertEqual(memory.total_bytes, 2 * _FakeTensor().nbytes)
        huge = _remembered(4)
        memory.store(huge, _FakeTensor((1, 400, 8)))
        self.assertEqual(list(memory.index), [huge.key], "the newest entry stays even when it alone exceeds the cap")

    def test_the_index_is_replaced_not_mutated(self) -> None:
        memory = pcs.EmbeddingMemory(1)
        memory.store(_remembered(1), _FakeTensor())
        before = memory.index
        memory.store(_remembered(2), _FakeTensor())
        self.assertEqual([item.text for item in before.values()], ["t1"], "a lock-free reader keeps a consistent view")
        self.assertEqual([item.text for item in memory.index.values()], ["t2"])

    def test_tensor_of_needs_the_same_encoder(self) -> None:
        memory = pcs.EmbeddingMemory(4)
        remembered = _remembered(1)
        tensor = _FakeTensor()
        memory.store(remembered, tensor)
        self.assertIs(memory.tensor_of(LIB, "t1", 4, remembered.encoder), tensor)
        fp8 = pl.EncoderProvenance(remembered.encoder.source, remembered.encoder.format, True, remembered.encoder.encoder_id, "x")
        self.assertIsNone(memory.tensor_of(LIB, "t1", 4, fp8))
        self.assertIsNone(memory.tensor_of(OTHER_LIB, "t1", 4, remembered.encoder))
        self.assertIsNone(memory.tensor_of(LIB, "t1", 8, remembered.encoder))

    def test_invalid_caps_are_refused(self) -> None:
        with self.assertRaises(ValueError):
            pcs.EmbeddingMemory(0)
        with self.assertRaises(ValueError):
            pcs.EmbeddingMemory(1, max_bytes=-1)


class SmallHelperTests(unittest.TestCase):
    """`tensor_bytes`, `tensor_width`, `same_encoder`, `memory_key`."""

    def test_tensor_bytes(self) -> None:
        class TorchLike:
            def numel(self) -> int:
                return 10

            def element_size(self) -> int:
                return 2

        self.assertEqual(pcs.tensor_bytes(TorchLike()), 20)
        self.assertEqual(pcs.tensor_bytes(_FakeTensor((1, 2, 3))), 12)
        self.assertEqual(pcs.tensor_bytes(object()), 0)

    def test_tensor_width(self) -> None:
        self.assertEqual(pcs.tensor_width(_FakeTensor((1, 4, 24))), 24)
        self.assertIsNone(pcs.tensor_width(object()))

    def test_same_encoder_is_fingerprint_and_fp8(self) -> None:
        current = _encoder()
        self.assertTrue(pl.same_encoder(_encoder(source="other", fmt="q8_0"), current), "source and format are not identity")
        self.assertFalse(pl.same_encoder(_encoder(fp8=True), current))
        self.assertFalse(pl.same_encoder(_encoder(encoder_id="b" * 64), current))
        self.assertFalse(pl.same_encoder(current, None))

    def test_memory_key(self) -> None:
        self.assertEqual(pcs.memory_key(LIB, "a", 4, _encoder(fp8=True), ""), (LIB, "a", 4, "a" * 64, "fp8", "encoded"))
        self.assertEqual(pcs.memory_key(LIB, "a", 4, _encoder(), "x.msprompt")[-1], "x.msprompt")


class PlanAndStatusTests(_Case):
    """THE selection over memory and the library."""

    def test_nothing_cached_encodes_every_text_in_order(self) -> None:
        plan = self.orchestrator.plan(self.request(texts=["a cat", ""]), blocking_load=True)
        self.assertEqual((dict(plan.served), plan.to_encode), ({}, ("a cat", "")))
        self.assertEqual(self.orchestrator.status(self.request()), (False, None))

    def test_memory_beats_a_file_of_the_same_precision_and_a_more_precise_file_beats_memory(self) -> None:
        self.add_entry(encoder=_encoder(fmt="q8_0", encoder_id="b" * 64))
        self.remember(encoder=_encoder(fmt="q8_0"))
        cached, info = self.orchestrator.status(self.request())
        self.assertTrue(cached)
        self.assertEqual((info["origin"], info["current_encoder"]), (pcs.MEMORY_ORIGIN, True), "same fingerprint and fp8 flag: the current encoder made it")
        self.add_entry(encoder=_encoder(fmt="bf16", encoder_id="c" * 64), created_at="2026-01-01T00:00:00Z")
        choice = self.orchestrator.plan(self.request(), blocking_load=True).served["a cat"]
        self.assertEqual((choice.source, choice.encoder.format), ("library", "bf16"))

    def test_a_width_the_model_does_not_take_is_dropped(self) -> None:
        self.add_entry(width=8)
        self.remember(width=8)
        plan = self.orchestrator.plan(self.request(width=24), blocking_load=True)
        self.assertEqual(plan.to_encode, ("a cat",))
        self.assertTrue(self.orchestrator.status(self.request(width=None))[0], "an unknown width filters nothing")

    def test_without_a_library_only_memory_of_no_library_serves(self) -> None:
        self.add_entry()
        self.remember(library=LIB)
        self.assertEqual(self.orchestrator.plan(self.request(library=None), blocking_load=True).to_encode, ("a cat",))
        self.remember(library="")
        self.assertTrue(self.orchestrator.status(self.request(library=None))[0])

    def test_status_reports_the_first_text_and_needs_every_text(self) -> None:
        entry = self.add_entry(name="Кот")
        cached, info = self.orchestrator.status(self.request(texts=["a cat", ""]))
        self.assertFalse(cached, "the second text is not served")
        self.assertEqual((info["file"], info["name"], info["current_encoder"]), (entry.file, "Кот", True))
        self.assertFalse(self.orchestrator.status(self.request(current="unidentifiable"))[1]["current_encoder"], "an unidentifiable encoder made nothing")


class ServeTests(_Case):
    """`serve_locked` and `remember_locked`."""

    def test_a_library_read_is_remembered_under_its_file(self) -> None:
        entry = self.add_entry()
        choice = self.orchestrator.plan(self.request(), blocking_load=True).served["a cat"]
        tensor = self.orchestrator.serve_locked(choice)
        self.assertEqual(tensor.shape, (1, 4, 8))
        (remembered,) = self.memory.index.values()
        self.assertEqual((remembered.entry, remembered.key[-1]), (entry, entry.file))
        again = self.orchestrator.plan(self.request(), blocking_load=True).served["a cat"]
        self.assertEqual(again.source, "memory")
        self.assertEqual(pcs.entry_info(again.entry, None)["file"], entry.file, "a memory copy of a file reports the file")

    def test_a_vanished_file_is_gone(self) -> None:
        entry = self.add_entry()
        choice = self.orchestrator.plan(self.request(), blocking_load=True).served["a cat"]
        self.adapter.library_of(LIB).path_of(entry.file).unlink()
        self.assertIsNone(self.orchestrator.serve_locked(choice))
        self.assertEqual(self.orchestrator.plan(self.request(), blocking_load=True).to_encode, ("a cat",))

    def test_an_evicted_memory_entry_is_gone(self) -> None:
        small = pcs.PromptCacheOrchestrator(self.adapter, pcs.EmbeddingMemory(1))
        small.remember_locked(LIB, "a cat", 4, _encoder(), _FakeTensor(), "2026-10-02T00:00:00Z")
        choice = small.plan(self.request(), blocking_load=True).served["a cat"]
        small.remember_locked(LIB, "a dog", 4, _encoder(), _FakeTensor(), "2026-10-02T00:00:00Z")
        self.assertIsNone(small.serve_locked(choice))


class BuildTests(_Case):
    """`build_locked`."""

    def _build(self, request: dict[str, Any], name: str | None = None) -> tuple[pcs.BuildResult, list[list[str]], list[bool]]:
        encoded: list[list[str]] = []
        nothing: list[bool] = []

        def encode(texts: list[str]) -> dict[str, Any]:
            encoded.append(list(texts))
            return {text: _FakeTensor() for text in texts}

        result = self.orchestrator.build_locked(request, name, encode, current=_encoder(), extra={"note": "x"}, on_nothing_to_encode=nothing.append)
        return result, encoded, nothing

    def test_it_encodes_what_memory_lacks_and_names_only_the_prompt(self) -> None:
        self.remember("")
        result, encoded, nothing = self._build(self.request(texts=["a cat", ""]), name="Кот")
        # `renamed` counts rewritten entries: a new entry is written under the name already.
        self.assertEqual((encoded, nothing, result.encoded, result.name, result.renamed), ([["a cat"]], [], True, "Кот", False))
        self.assertEqual({(entry.prompt, entry.name) for entry in result.written}, {("a cat", "Кот"), ("", "")})
        header = read_safetensors_header(self.adapter.library_of(LIB).path_of(result.written[0].file))
        self.assertEqual(header["__metadata__"]["note"], "x")

    def test_an_already_saved_prompt_writes_nothing_and_a_new_name_renames(self) -> None:
        self._build(self.request(), name="Кот")
        again, encoded, nothing = self._build(self.request())
        self.assertEqual((again.written, again.name, again.renamed, encoded, nothing), ((), "Кот", False, [], [False]))
        renamed, _encoded, _nothing = self._build(self.request(), name="Котик")
        self.assertEqual((renamed.written, renamed.name, renamed.renamed), ((), "Котик", True))

    def test_an_entry_of_another_encoder_does_not_count_as_saved(self) -> None:
        self.add_entry(encoder=_encoder(encoder_id="b" * 64), name="Старое")
        self.remember()
        result, encoded, nothing = self._build(self.request())
        self.assertEqual((encoded, nothing, result.name, len(result.written)), ([], [True], "Старое", 1))


class LibrarySurfaceTests(_Case):
    """`listing`, `export`, `import_file`, `require_library`."""

    def test_the_listing(self) -> None:
        self.add_entry("older", name="Old", created_at="2026-01-01T00:00:00Z")
        self.add_entry("newer", encoder=_encoder(fmt="q8_0", encoder_id="b" * 64), created_at="2026-03-01T00:00:00Z")
        self.add_entry("newer", created_at="2026-02-01T00:00:00Z")
        self.add_entry("", created_at="2026-04-01T00:00:00Z")
        self.add_entry("wide", width=24)
        self.adapter.legacy = {"migrated": 1}
        listing = self.orchestrator.listing(self.request())
        self.assertEqual([row["prompt"] for row in listing["texts"]], ["newer", "older"], "no empty text, no other width")
        self.assertEqual([entry["encoder_format"] for entry in listing["texts"][0]["entries"]], ["bf16", "q8_0"])
        self.assertEqual((listing["library"], listing["text_encoder_available"], listing["legacy"]), (LIB, True, {"migrated": 1}))
        self.assertEqual(self.adapter.migrations, [LIB], "the listing migrates first")
        self.assertFalse(self.orchestrator.listing(self.request(current="unidentifiable"))["texts"][0]["selected"]["current_encoder"], "browsing never fails over an unidentifiable encoder")

    def test_a_request_without_a_library_is_refused_naming_the_owner(self) -> None:
        for call in (lambda: self.orchestrator.listing(self.request(library=None)), lambda: self.orchestrator.export(self.request(library=None), "x.msprompt", str(self.root / "x.msprompt"))):
            with self.subTest(), self.assertRaisesRegex(ValueError, "^Fake model: в запросе нет «prompt_library»"):
                call()

    def test_export_copies_the_file(self) -> None:
        entry = self.add_entry()
        dest = self.root / "out.msprompt"
        out = self.orchestrator.export(self.request(), entry.file, str(dest))
        self.assertEqual(out, {"library": LIB, "file": entry.file, "path": str(dest), "size_bytes": entry.size_bytes})
        self.assertEqual(dest.read_bytes(), self.adapter.library_of(LIB).path_of(entry.file).read_bytes())

    def _outside(self, library_id: str, *, marker: str | None = None) -> Path:
        metadata = pl.build_metadata(_spec(library_id), prompt="imported", name="Imp", max_sequence_length=4, encoder=_encoder(), origin=pl.ORIGIN_BUILT)
        if marker is not None:
            metadata["format"] = marker
        path = self.root / "outside" / f"{library_id}-{marker}.msprompt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(_container(metadata, _FakeTensor()))
        return path

    def test_a_v2_file_lands_in_its_own_library(self) -> None:
        out = self.orchestrator.import_file(self.request(), str(self._outside(OTHER_LIB)))
        self.assertEqual((out["library"], out["library_matches"], out["already_present"], out["origin"], out["name"]), (OTHER_LIB, False, False, pl.ORIGIN_IMPORTED, "Imp"))
        self.assertTrue(self.orchestrator.import_file(self.request(library=OTHER_LIB), str(self._outside(OTHER_LIB)))["already_present"])
        self.assertEqual(self.adapter.migrations, [LIB, OTHER_LIB], "an import migrates first")

    def test_a_foreign_library_and_an_unknown_marker_are_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "относится к библиотеке «somebody_else», а не к Fake model"):
            self.orchestrator.import_file(self.request(), str(self._outside("somebody_else")))
        with self.assertRaisesRegex(ValueError, "нет известной метки формата"):
            self.orchestrator.import_file(self.request(), str(self._outside(LIB, marker="manhwastudio.something_old")))


class LibraryFieldsTests(unittest.TestCase):
    """`read_library_fields`: shapes, defaults and the owner prefix."""

    def test_values_and_defaults(self) -> None:
        self.assertEqual(pcs.read_library_fields(None, owner="M"), (None, "", ()))
        self.assertEqual(pcs.read_library_fields({"prompt_library": LIB, "text_encoder_source": "stock", "prompt_cache_precision_order": ["bf16", "fp8"]}, owner="M"), (LIB, "stock", ("bf16", "fp8")))
        self.assertEqual(pcs.read_library_fields({"text_encoder_source": ""}, owner="M")[1], "")

    def test_refusals_name_the_owner_and_the_field(self) -> None:
        for params, field in (
            ({"prompt_library": "Bad-Id"}, "prompt_library"),
            ({"text_encoder_source": "Stock"}, "text_encoder_source"),
            ({"prompt_cache_precision_order": "bf16"}, "prompt_cache_precision_order"),
            ({"prompt_cache_precision_order": ["BF16"]}, r"prompt_cache_precision_order\[0\]"),
            ({"prompt_cache_precision_order": ["bf16", "bf16"]}, "повторы"),
        ):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, f"^Owner X: .*{field}"):
                pcs.read_library_fields(params, owner="Owner X")

    def test_an_empty_prompt_is_not_cached(self) -> None:
        with self.assertRaisesRegex(ValueError, "Кэшировать нечего"):
            pcs.require_prompt_to_cache("")
        pcs.require_prompt_to_cache("a cat")


if __name__ == "__main__":
    unittest.main()
