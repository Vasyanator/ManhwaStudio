"""
File: modules/ai_backend/runtime/test_prompt_library.py

Purpose:
Unit tests for `runtime/prompt_library.py`: the selection rule, deterministic
names, the header-only metadata rewrite, the reconcile of a library directory
against its files and its index, atomic publishing, the lock-free snapshot,
writes / adds / renames and the validated tensor read; the preset-name rule
against the fixture shared with the Rust client
(`fixtures/preset_name_normalization.json`); the containment rule (no symbolic
link is ever read for a rewrite, replaced, written through or deleted) and the
durability order (directory fsync after each replace / unlink).

Notes:
- Stdlib only: `serialize_tensor` / `load_tensor` are patched with a stub
  container writer over `_FakeTensor`; the one real safetensors round trip is
  skip-guarded on torch + safetensors.
- Every test works in its own temporary directory; nothing is written into the
  repository or the program root.
"""

from __future__ import annotations

import importlib.util
import json
import os
import struct
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.runtime import prompt_library as pl
from modules.ai_backend.runtime.safetensors_stream import read_safetensors_header

SPEC = pl.LibrarySpec("test_model", "prompt_embeds", "bfloat16", "BF16")


class _FakeTensor:
    """A tensor stand-in: a shape and the bytes it serializes to."""

    def __init__(self, shape: tuple[int, ...] = (1, 4, 8), fill: int = 7) -> None:
        self.shape = shape
        size = 2
        for dim in shape:
            size *= dim
        self.data = bytes([fill]) * size


def _container(metadata: dict[str, str], tensor: _FakeTensor, *, name: str = "prompt_embeds", dtype: str = "BF16") -> bytes:
    """A real safetensors container (stdlib-built) of one tensor."""
    header = {name: {"dtype": dtype, "shape": list(tensor.shape), "data_offsets": [0, len(tensor.data)]}, "__metadata__": metadata}
    blob = json.dumps(header).encode("utf-8")
    blob += b" " * (-(8 + len(blob)) % 8)
    return struct.pack("<Q", len(blob)) + blob + tensor.data


def _fake_serialize(tensor_name: str, tensor: _FakeTensor, metadata: dict[str, str]) -> bytes:
    return _container(dict(metadata), tensor, name=tensor_name)


def _fake_load(path: Path, tensor_name: str) -> _FakeTensor:
    header = read_safetensors_header(path)
    return _FakeTensor(tuple(header[tensor_name]["shape"]))


def _encoder(source: str = "stock", fmt: str = "bf16", fp8: bool = False, encoder_id: str = "a" * 64, label: str = "text_encoder") -> pl.EncoderProvenance:
    return pl.EncoderProvenance(source, fmt, fp8, encoder_id, label)


class _LibraryCase(unittest.TestCase):
    """A temp root, the stub serializer, and a fresh (unregistered) library."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        for name, replacement in (("serialize_tensor", _fake_serialize), ("load_tensor", _fake_load)):
            attr = patch.object(pl, name, replacement)
            attr.start()
            self.addCleanup(attr.stop)
        self.library = pl.PromptLibrary(self.root, SPEC)

    def write(self, prompt: str = "remove the text", *, encoder: pl.EncoderProvenance | None = None, msl: int = 512, name: str = "preset", shape: tuple[int, ...] = (1, 4, 8)) -> pl.LibraryEntry:
        entry, written = self.library.write_entry(prompt, _FakeTensor(shape), encoder or _encoder(), max_sequence_length=msl, name=name, origin=pl.ORIGIN_BUILT)
        self.assertTrue(written)
        return entry

    def index(self) -> dict[str, Any]:
        return json.loads((self.library.directory / pl.INDEX_FILE).read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------
class _Candidate:
    def __init__(self, file: str, encoder: pl.EncoderProvenance, created: str = "2026-01-01T00:00:00Z", memory: bool = False) -> None:
        self.file, self.encoder, self.created, self.memory = file, encoder, created, memory


def _pick(candidates: list[_Candidate], pref: pl.Preference) -> str | None:
    chosen = pl.select_entry(candidates, pref, provenance=lambda c: c.encoder, created=lambda c: c.created, in_memory=lambda c: c.memory, name=lambda c: c.file)
    return None if chosen is None else chosen.file


ORDER = ("bf16", "q8_0", "fp8", "q6_k", "q5_k_m", "q4_k_m", "gguf")


class SelectionTests(unittest.TestCase):
    def test_the_selected_source_beats_a_more_precise_format_of_the_other(self) -> None:
        candidates = [_Candidate("stock_bf16", _encoder("stock", "bf16")), _Candidate("unc_q8", _encoder("uncensored", "q8_0"))]
        self.assertEqual(_pick(candidates, pl.Preference("uncensored", ORDER)), "unc_q8")
        self.assertEqual(_pick(candidates, pl.Preference("stock", ORDER)), "stock_bf16")

    def test_the_other_source_serves_when_it_is_all_there_is(self) -> None:
        self.assertEqual(_pick([_Candidate("stock_q4", _encoder("stock", "q4_k_m"))], pl.Preference("uncensored", ORDER)), "stock_q4")

    def test_within_a_source_the_more_precise_format_wins(self) -> None:
        candidates = [_Candidate("q4", _encoder(fmt="q4_k_m")), _Candidate("fp8", _encoder(fmt="bf16", fp8=True)), _Candidate("q8", _encoder(fmt="q8_0"))]
        self.assertEqual(_pick(candidates, pl.Preference("stock", ORDER)), "q8")
        self.assertEqual(_pick(candidates[:2], pl.Preference("stock", ORDER)), "fp8", "fp8 ranks between q8_0 and q6_k")

    def test_an_unknown_precision_ranks_last(self) -> None:
        candidates = [_Candidate("odd", _encoder(fmt="weird")), _Candidate("gguf", _encoder(fmt="gguf"))]
        self.assertEqual(_pick(candidates, pl.Preference("stock", ORDER)), "gguf")

    def test_memory_then_newest_then_name(self) -> None:
        same = _encoder()
        self.assertEqual(_pick([_Candidate("b_file", same), _Candidate("a_mem", same, memory=True)], pl.Preference("stock", ORDER)), "a_mem")
        self.assertEqual(_pick([_Candidate("old", same, "2026-01-01T00:00:00Z"), _Candidate("new", same, "2026-02-01T00:00:00Z")], pl.Preference("stock", ORDER)), "new")
        self.assertEqual(_pick([_Candidate("b", same), _Candidate("a", same)], pl.Preference("stock", ORDER)), "a")

    def test_an_empty_source_preference_starts_at_precision(self) -> None:
        candidates = [_Candidate("unc_q4", _encoder("uncensored", "q4_k_m")), _Candidate("stock_q8", _encoder("stock", "q8_0"))]
        self.assertEqual(_pick(candidates, pl.Preference("", ORDER)), "stock_q8")
        # An entry of unknown source is never "preferred" by an empty preference.
        self.assertEqual(_pick([_Candidate("x", _encoder("", "q4_k_m")), _Candidate("y", _encoder("stock", "bf16"))], pl.Preference("", ORDER)), "y")

    def test_no_candidate_is_none(self) -> None:
        self.assertIsNone(_pick([], pl.Preference("stock", ORDER)))

    def test_an_unparsable_date_is_the_oldest(self) -> None:
        same = _encoder()
        self.assertEqual(_pick([_Candidate("garbage", same, "yesterday"), _Candidate("dated", same, "2020-01-01T00:00:00Z")], pl.Preference("stock", ORDER)), "dated")


# ---------------------------------------------------------------------------
# Names
# ---------------------------------------------------------------------------
class NameTests(unittest.TestCase):
    def test_file_names_are_deterministic_and_carry_every_key(self) -> None:
        name = pl.entry_file_name("Remove the text!", 512, _encoder(encoder_id="0123456789abcdef"))
        self.assertEqual(name, pl.entry_file_name("Remove the text!", 512, _encoder(encoder_id="0123456789abcdef")))
        self.assertTrue(name.startswith("Remove_the_text-"))
        self.assertTrue(name.endswith("-01234567.msprompt"))
        self.assertNotEqual(name, pl.entry_file_name("Remove the text!", 256, _encoder(encoder_id="0123456789abcdef")))
        self.assertNotEqual(name, pl.entry_file_name("Remove the text?", 512, _encoder(encoder_id="0123456789abcdef")), "same slug, other hash")
        self.assertTrue(pl.entry_file_name("x", 512, _encoder(fp8=True)).endswith("-fp8.msprompt"))
        self.assertTrue(pl.entry_file_name("", 512, _encoder()).startswith("empty-"))

    def test_no_prompt_can_escape_the_directory(self) -> None:
        for prompt in ("../../etc/passwd", "a/b\\c", "C:\\x", "\x00\n..", "CON"):
            with self.subTest(prompt=prompt):
                name = pl.entry_file_name(prompt, 512, _encoder(encoder_id="../.."))
                self.assertEqual(Path(name).name, name)
                self.assertNotIn("/", name)
                self.assertNotIn("\\", name)
                self.assertNotIn("..", name.removesuffix(pl.ENTRY_SUFFIX))
                pl.require_entry_file_name(name)

    def test_a_client_file_name_must_be_bare(self) -> None:
        for bad in ("", "../x.msprompt", "a/b.msprompt", "x.txt", ".msprompt", "/abs.msprompt"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                pl.require_entry_file_name(bad)

    def test_preset_names(self) -> None:
        self.assertEqual(pl.normalize_preset_name("  a\nb\x00c  "), "a b c")
        self.assertEqual(len(pl.normalize_preset_name("x" * 500)), pl.MAX_PRESET_NAME_CHARS)
        self.assertEqual(pl.default_preset_name("short  prompt\n"), "short prompt")
        long = pl.default_preset_name("word " * 30)
        self.assertTrue(long.endswith("…"))
        self.assertLessEqual(len(long), pl.DEFAULT_NAME_CHARS + 1)
        self.assertEqual(pl.default_preset_name(""), "")

    def test_library_ids(self) -> None:
        self.assertEqual(pl.require_library_id("flux2_klein_9b"), "flux2_klein_9b")
        for bad in ("", "Flux", "a-b", "x" * 41, "../x"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                pl.require_library_id(bad)


# ---------------------------------------------------------------------------
# The container
# ---------------------------------------------------------------------------
class ContainerTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def _metadata(self, **overrides: str) -> dict[str, str]:
        metadata = pl.build_metadata(SPEC, prompt="p", name="n", max_sequence_length=512, encoder=_encoder(), origin=pl.ORIGIN_BUILT, created_at="2026-01-01T00:00:00Z")
        metadata.update(overrides)
        return metadata

    def test_the_rewrite_keeps_the_tensor_bytes_and_parses(self) -> None:
        tensor = _FakeTensor((1, 3, 5), fill=42)
        original = _container({"format": "x"}, tensor)
        rewritten = pl.rewrite_safetensors_metadata(original, self._metadata(name="новое имя"))
        self.assertEqual(rewritten[-len(tensor.data):], tensor.data)
        header_len = struct.unpack("<Q", rewritten[:8])[0]
        self.assertEqual((8 + header_len) % 8, 0, "the data section stays 8-byte aligned")
        path = self.root / "x.msprompt"
        path.write_bytes(rewritten)
        header = read_safetensors_header(path)
        self.assertEqual(header["__metadata__"]["name"], "новое имя")
        entry = pl.parse_entry_header(header, SPEC, file="x.msprompt", size_bytes=len(rewritten), mtime_ns=1)
        self.assertEqual((entry.prompt, entry.shape, entry.encoder.source), ("p", (1, 3, 5), "stock"))

    def test_a_malformed_container_is_refused(self) -> None:
        good = _container({}, _FakeTensor())
        for bad in (b"", b"\x00" * 7, struct.pack("<Q", 10**9) + b"{}", good[:-1], struct.pack("<Q", 2) + b"[]"):
            with self.subTest(bad=bad[:12]), self.assertRaises(ValueError):
                pl.rewrite_safetensors_metadata(bad, {})

    def test_reserved_extra_keys_are_refused(self) -> None:
        with self.assertRaises(ValueError):
            pl.build_metadata(SPEC, prompt="p", name="", max_sequence_length=1, encoder=_encoder(), origin=pl.ORIGIN_BUILT, extra={"prompt": "x"})

    def test_every_header_refusal_names_its_reason(self) -> None:
        cases = {
            "format": {"format": "manhwastudio.flux2_klein.prompt_cache"},
            "newer": {"format_version": "3"},
            "library": {"library": "other"},
            "dtype": {"dtype": "float16"},
            "length": {"max_sequence_length": "0"},
            "fp8": {"encoder_fp8": "maybe"},
            "source": {"encoder_source": "Bad Source"},
            "id": {"encoder_id": ""},
        }
        for label, override in cases.items():
            with self.subTest(label), self.assertRaises(ValueError):
                header, _start = pl.parse_payload_header(_container(self._metadata(**override), _FakeTensor()))
                pl.parse_entry_header(header, SPEC, file="x.msprompt", size_bytes=1, mtime_ns=1)
        tensor_header = {"other": {"dtype": "BF16", "shape": [1], "data_offsets": [0, 2]}, "__metadata__": self._metadata()}
        with self.assertRaisesRegex(ValueError, "ровно один тензор"):
            pl.parse_entry_header(tensor_header, SPEC, file="x.msprompt", size_bytes=1, mtime_ns=1)
        token_header = {"prompt_embeds": {"dtype": "F16", "shape": [1], "data_offsets": [0, 2]}, "__metadata__": self._metadata()}
        with self.assertRaises(ValueError):
            pl.parse_entry_header(token_header, SPEC, file="x.msprompt", size_bytes=1, mtime_ns=1)


# ---------------------------------------------------------------------------
# The library directory
# ---------------------------------------------------------------------------
class ReconcileTests(_LibraryCase):
    def test_an_absent_directory_is_an_empty_library_and_creates_nothing(self) -> None:
        snap = self.library.load(blocking=True)
        self.assertEqual(dict(snap.entries), {})
        self.assertFalse(self.library.directory.exists())

    def test_a_write_is_listed_and_indexed(self) -> None:
        entry = self.write()
        snap = self.library.snapshot()
        self.assertEqual(dict(snap.entries), {entry.file: entry})
        self.assertEqual(self.library.entries_for("remove the text", 512), (entry,))
        self.assertEqual(self.library.entries_for("remove the text", 256), ())
        row = self.index()["entries"][entry.file]
        self.assertEqual((row["prompt"], row["name"], row["encoder_source"], row["shape"]), ("remove the text", "preset", "stock", [1, 4, 8]))

    def test_a_new_file_dropped_in_is_found_by_load(self) -> None:
        self.write("a")
        other = pl.PromptLibrary(self.root, SPEC)
        other.write_entry("b", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
        self.assertEqual(sorted(self.library.load(blocking=True).texts()), ["a", "b"])

    def test_a_removed_file_drops_its_row(self) -> None:
        entry = self.write()
        self.library.path_of(entry.file).unlink()
        self.assertEqual(dict(self.library.reconcile().entries), {})
        self.assertEqual(self.index()["entries"], {})

    def test_a_changed_file_is_read_again(self) -> None:
        entry = self.write(name="first")
        path = self.library.path_of(entry.file)
        payload = path.read_bytes()
        header = json.loads(payload[8 : 8 + struct.unpack("<Q", payload[:8])[0]])
        metadata = dict(header["__metadata__"], name="second")
        path.write_bytes(pl.rewrite_safetensors_metadata(payload, metadata))
        self.assertEqual(self.library.reconcile().entries[entry.file].name, "second")

    def test_a_same_size_rewrite_is_caught_by_its_mtime(self) -> None:
        entry = self.write(name="aaaa")
        path = self.library.path_of(entry.file)
        payload = path.read_bytes()
        self.assertIn(b'"aaaa"', payload)
        path.write_bytes(payload.replace(b'"aaaa"', b'"bbbb"'))
        os.utime(path, ns=(entry.mtime_ns + 10**9, entry.mtime_ns + 10**9))
        self.assertEqual(self.library.reconcile().entries[entry.file].name, "bbbb")

    def test_a_corrupt_or_foreign_file_is_skipped_never_fatal(self) -> None:
        good = self.write()
        (self.library.directory / "junk.msprompt").write_bytes(b"not a container")
        (self.library.directory / "foreign.msprompt").write_bytes(_container({"format": "x"}, _FakeTensor()))
        snap = self.library.reconcile()
        self.assertEqual(list(snap.entries), [good.file])
        self.assertEqual(set(snap.skipped), {"junk.msprompt", "foreign.msprompt"})
        self.assertIn("junk.msprompt", self.index()["skipped"])

    def test_staging_and_other_files_are_ignored(self) -> None:
        self.write()
        (self.library.directory / "x.msprompt.123.part").write_bytes(b"partial")
        (self.library.directory / "notes.txt").write_text("hi", encoding="utf-8")
        (self.library.directory / "dir.msprompt").mkdir()
        snap = self.library.reconcile()
        self.assertEqual(len(snap.entries), 1)
        self.assertEqual(dict(snap.skipped), {})

    def test_a_lost_malformed_or_newer_index_is_rebuilt(self) -> None:
        entry = self.write()
        for content in (None, b"{ not json", json.dumps({"format": pl.INDEX_FORMAT, "version": 99, "library": SPEC.library_id, "entries": {}, "skipped": {}}).encode(), json.dumps({"format": pl.INDEX_FORMAT, "version": 1, "library": "other", "entries": {}, "skipped": {}}).encode()):
            with self.subTest(content=content):
                index = self.library.directory / pl.INDEX_FILE
                if content is None:
                    index.unlink()
                else:
                    index.write_bytes(content)
                fresh = pl.PromptLibrary(self.root, SPEC)
                with self.assertLogs(pl.log, "WARNING") if content is not None else _no_op():
                    snap = fresh.load(blocking=True)
                self.assertEqual(list(snap.entries), [entry.file])
                self.assertEqual(list(self.index()["entries"]), [entry.file])

    def test_a_valid_index_is_trusted_without_reading_headers(self) -> None:
        entry = self.write()
        fresh = pl.PromptLibrary(self.root, SPEC)
        with patch.object(pl, "read_safetensors_header", side_effect=AssertionError("header read")):
            snap = fresh.load(blocking=True)
        self.assertEqual(snap.entries[entry.file], entry)

    def test_nothing_changed_writes_no_index(self) -> None:
        self.write()
        index = self.library.directory / pl.INDEX_FILE
        before = index.stat().st_mtime_ns
        with patch.object(pl, "publish_bytes_atomically", side_effect=AssertionError("rewrote")):
            self.library.reconcile()
            self.library.load(blocking=True)
        self.assertEqual(index.stat().st_mtime_ns, before)

    def test_a_non_blocking_load_returns_the_old_snapshot_while_the_lock_is_held(self) -> None:
        self.write("a")
        old = self.library.snapshot()
        pl.PromptLibrary(self.root, SPEC).write_entry("b", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
        acquired = self.library._lock.acquire()
        self.assertTrue(acquired)
        try:
            self.assertIs(self.library.load(blocking=False), old)
        finally:
            self.library._lock.release()
        self.assertEqual(sorted(self.library.load(blocking=False).texts()), ["a", "b"])

    def test_texts_and_preset_names(self) -> None:
        with patch.object(pl, "utc_now", return_value="2026-01-01T00:00:00Z"):
            self.write("older", name="Old")
        with patch.object(pl, "utc_now", return_value="2026-03-01T00:00:00Z"):
            self.write("newer", name="")
            self.write("newer", encoder=_encoder("uncensored", "q8_0", encoder_id="b" * 64), name="")
        with patch.object(pl, "utc_now", return_value="2026-02-01T00:00:00Z"):
            self.write("newer", encoder=_encoder("stock", "q4_k_m", encoder_id="c" * 64), name="Named")
        snap = self.library.snapshot()
        self.assertEqual(snap.texts(), ["newer", "older"])
        self.assertEqual(snap.preset_name("newer"), "Named", "the newest NON-EMPTY name")
        self.assertEqual(snap.preset_name("older"), "Old")
        self.assertEqual(len(snap.entries_of_text("newer")), 3)


class _no_op:
    def __enter__(self) -> None:
        return None

    def __exit__(self, *_exc: object) -> None:
        return None


class WriteTests(_LibraryCase):
    def test_a_repeated_write_is_idempotent(self) -> None:
        entry = self.write()
        again, written = self.library.write_entry("remove the text", _FakeTensor(), _encoder(), max_sequence_length=512, name="other", origin=pl.ORIGIN_BUILT)
        self.assertFalse(written)
        self.assertEqual(again, entry)

    def test_a_foreign_entry_at_the_deterministic_name_is_never_overwritten(self) -> None:
        entry = self.write()
        payload = self.library.path_of(entry.file).read_bytes()
        header = json.loads(payload[8 : 8 + struct.unpack("<Q", payload[:8])[0]])
        foreign = pl.rewrite_safetensors_metadata(payload, dict(header["__metadata__"], prompt="something else"))
        self.library.path_of(entry.file).write_bytes(foreign)
        with self.assertRaisesRegex(ValueError, "занят"):
            self.library.write_entry("remove the text", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)

    def test_a_failed_write_names_the_path_and_leaves_no_staging_file(self) -> None:
        self.write("a")
        before = self.index()
        with patch.object(pl.os, "replace", side_effect=OSError("disk full")), self.assertRaisesRegex(ValueError, "disk full"):
            self.write("b")
        self.assertEqual([path.name for path in self.library.directory.iterdir() if path.name.endswith(".part")], [])
        self.assertEqual(self.index(), before)
        self.assertEqual(self.library.snapshot().texts(), ["a"])

    def test_add_payload_keeps_the_tensor_bytes(self) -> None:
        tensor = _FakeTensor(fill=99)
        metadata = pl.build_metadata(SPEC, prompt="imported", name="Imp", max_sequence_length=512, encoder=_encoder("uncensored"), origin=pl.ORIGIN_IMPORTED, extra={"migrated_from": "fam/x.msprompt"})
        entry, written = self.library.add_payload(_container({"anything": "old"}, tensor), metadata)
        self.assertTrue(written)
        stored = self.library.path_of(entry.file).read_bytes()
        self.assertEqual(stored[-len(tensor.data):], tensor.data)
        self.assertEqual(read_safetensors_header(self.library.path_of(entry.file))["__metadata__"]["migrated_from"], "fam/x.msprompt")
        self.assertEqual((entry.origin, entry.name), (pl.ORIGIN_IMPORTED, "Imp"))
        self.assertFalse(self.library.add_payload(_container({}, tensor), metadata)[1], "idempotent")

    def test_add_payload_refuses_another_library(self) -> None:
        other = pl.LibrarySpec("other_model", "prompt_embeds", "bfloat16", "BF16")
        metadata = pl.build_metadata(other, prompt="x", name="", max_sequence_length=512, encoder=_encoder(), origin=pl.ORIGIN_IMPORTED)
        with self.assertRaisesRegex(ValueError, "другой библиотеки"):
            self.library.add_payload(_container({}, _FakeTensor()), metadata)

    def test_rename_rewrites_every_entry_of_the_text_only(self) -> None:
        a = self.write("p", name="old")
        b = self.write("p", encoder=_encoder("uncensored", encoder_id="d" * 64), msl=256, name="old")
        c = self.write("q", name="keep")
        data_before = self.library.path_of(a.file).read_bytes()[-64:]
        changed = self.library.rename_text("p", "  new\nname ")
        self.assertEqual({entry.file for entry in changed}, {a.file, b.file})
        snap = self.library.snapshot()
        self.assertEqual({snap.entries[a.file].name, snap.entries[b.file].name}, {"new name"})
        self.assertEqual(snap.entries[c.file].name, "keep")
        self.assertEqual(self.library.path_of(a.file).read_bytes()[-64:], data_before)
        self.assertEqual(self.library.rename_text("p", "new name"), (), "already named")


class ReadTensorTests(_LibraryCase):
    def test_a_valid_entry_reads(self) -> None:
        entry = self.write(shape=(1, 2, 3))
        self.assertEqual(self.library.read_tensor(entry).shape, (1, 2, 3))

    def test_a_vanished_file_is_gone_and_its_row_dropped(self) -> None:
        entry = self.write()
        self.library.path_of(entry.file).unlink()
        with self.assertRaises(pl.LibraryEntryGone):
            self.library.read_tensor(entry)
        self.assertNotIn(entry.file, self.library.snapshot().entries)

    def test_a_corrupted_file_is_gone_and_skipped(self) -> None:
        entry = self.write()
        self.library.path_of(entry.file).write_bytes(b"broken")
        with self.assertRaises(pl.LibraryEntryGone):
            self.library.read_tensor(entry)
        self.assertIn(entry.file, self.library.snapshot().skipped)

    def test_a_shape_that_disagrees_with_the_header_is_gone(self) -> None:
        entry = self.write()
        with patch.object(pl, "load_tensor", return_value=_FakeTensor((9,))), self.assertRaises(pl.LibraryEntryGone):
            self.library.read_tensor(entry)


# ---------------------------------------------------------------------------
# Preset-name normalization parity with the Rust client
# ---------------------------------------------------------------------------
NAME_FIXTURE = Path(__file__).resolve().parent / "fixtures" / "preset_name_normalization.json"


class NameNormalizationParityTests(unittest.TestCase):
    """The shared fixture pins the one rule both sides implement (the Rust test reads the same file)."""

    def setUp(self) -> None:
        self.fixture = json.loads(NAME_FIXTURE.read_text(encoding="utf-8"))

    def test_the_table_and_cap_are_the_fixture_s(self) -> None:
        self.assertEqual([tuple(pair) for pair in self.fixture["removed_ranges"]], list(pl.NAME_REMOVED_RANGES))
        self.assertEqual(self.fixture["max_chars"], pl.MAX_PRESET_NAME_CHARS)

    def test_every_scalar_value_is_classified_by_the_table_alone(self) -> None:
        ranges = [tuple(pair) for pair in self.fixture["removed_ranges"]]
        removed = bytearray(0x110000)
        for low, high in ranges:
            removed[low : high + 1] = b"\x01" * (high - low + 1)
        mismatches = [code for code in range(0x110000) if not 0xD800 <= code <= 0xDFFF and pl.is_name_removed_char(chr(code)) != bool(removed[code])]
        self.assertEqual(mismatches[:10], [])

    def test_every_fixture_case(self) -> None:
        for case in self.fixture["cases"]:
            with self.subTest(why=case["why"]):
                self.assertEqual(pl.normalize_preset_name(case["input"]), case["expected"])

    def test_a_lone_surrogate_is_replaced(self) -> None:
        self.assertEqual(pl.normalize_preset_name("a\ud800b"), "a b")


# ---------------------------------------------------------------------------
# Containment (no link is ever followed) and durability order
# ---------------------------------------------------------------------------
def _symlink_or_skip(case: unittest.TestCase, link: Path, target: Path, *, directory: bool) -> None:
    """Create `link -> target`, skipping the test where the platform refuses symbolic links (Windows without the privilege)."""
    try:
        os.symlink(target, link, target_is_directory=directory)
    except (OSError, NotImplementedError) as exc:
        case.skipTest(f"symbolic links unavailable: {exc}")


class ContainmentTests(_LibraryCase):
    def setUp(self) -> None:
        super().setUp()
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        self.outside = Path(outside.name)
        self.victim = self.outside / "victim.msprompt"
        self.victim.write_bytes(b"not yours")

    def test_dot_dot_and_outside_paths_are_refused(self) -> None:
        self.write()
        for bad in (self.library.directory / ".." / ".." / self.outside.name / "victim.msprompt", self.victim, self.root):
            with self.subTest(bad=str(bad)), self.assertRaises(pl.UnsafeLibraryPath):
                pl.require_confined(bad, self.root, directory=False)
        with self.assertRaises(pl.UnsafeLibraryPath):
            pl.remove_confined_file(self.root / ".." / self.outside.name / "victim.msprompt", self.root)
        self.assertEqual(self.victim.read_bytes(), b"not yours")

    def test_a_linked_entry_file_is_skipped_never_read_nor_replaced(self) -> None:
        entry = self.write("kept")
        _symlink_or_skip(self, self.library.directory / "linked.msprompt", self.victim, directory=False)
        snap = self.library.reconcile()
        self.assertEqual(snap.skipped.get("linked.msprompt"), pl.LINK_SKIP_REASON)
        self.assertEqual(set(snap.entries), {entry.file})
        with self.assertRaises(pl.UnsafeLibraryPath):
            self.library.confined_entry_path("linked.msprompt")
        with self.assertRaises(pl.UnsafeLibraryPath):
            pl.remove_confined_file(self.library.directory / "linked.msprompt", self.root)
        self.assertTrue((self.library.directory / "linked.msprompt").is_symlink())
        self.assertEqual(self.victim.read_bytes(), b"not yours")

    def test_a_write_onto_a_linked_name_is_refused(self) -> None:
        file = pl.entry_file_name("remove the text", 512, _encoder())
        self.library.directory.mkdir(parents=True)
        _symlink_or_skip(self, self.library.directory / file, self.victim, directory=False)
        with self.assertRaises(pl.UnsafeLibraryPath):
            self.library.write_entry("remove the text", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
        self.assertTrue((self.library.directory / file).is_symlink())
        self.assertEqual(self.victim.read_bytes(), b"not yours")

    def test_an_entry_swapped_for_a_link_is_neither_renamed_nor_read(self) -> None:
        entry = self.write("p", name="old")
        path = self.library.path_of(entry.file)
        path.unlink()
        _symlink_or_skip(self, path, self.victim, directory=False)
        with self.assertRaises(pl.LibraryEntryGone):
            self.library.read_tensor(entry)
        self.assertEqual(self.library.rename_text("p", "new"), (), "a linked entry is no entry")
        self.assertEqual(self.victim.read_bytes(), b"not yours")

    def test_a_linked_library_directory_is_not_listed_and_not_written(self) -> None:
        (self.outside / "victim2.msprompt").write_bytes(b"x")
        _symlink_or_skip(self, self.library.directory, self.outside, directory=True)
        snap = self.library.reconcile()
        self.assertEqual((dict(snap.entries), dict(snap.skipped)), ({}, {}))
        with self.assertRaises(pl.UnsafeLibraryPath):
            self.write()
        self.assertEqual(sorted(path.name for path in self.outside.iterdir()), ["victim.msprompt", "victim2.msprompt"], "nothing written through the link")

    def test_a_linked_root_is_not_written(self) -> None:
        linked_root = self.root / "linked_root"
        _symlink_or_skip(self, linked_root, self.outside, directory=True)
        library = pl.PromptLibrary(linked_root, SPEC)
        with self.assertRaises(pl.UnsafeLibraryPath):
            library.write_entry("x", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
        self.assertFalse((self.outside / SPEC.library_id).exists())

    def test_a_link_at_the_staging_name_is_never_written_through(self) -> None:
        dest = self.root / "out.msprompt"
        _symlink_or_skip(self, dest.with_name(f"{dest.name}.{os.getpid()}.part"), self.victim, directory=False)
        pl.publish_bytes_atomically(dest, b"payload")
        self.assertEqual(dest.read_bytes(), b"payload")
        self.assertEqual(self.victim.read_bytes(), b"not yours")


class DurabilityOrderTests(_LibraryCase):
    """Every publish fsyncs its directory after the replace; every confined removal fsyncs after the unlink."""

    def record(self) -> list[tuple[str, str]]:
        events: list[tuple[str, str]] = []
        real_replace, real_unlink, real_rmdir = os.replace, os.unlink, os.rmdir

        def replace(src: Any, dst: Any) -> None:
            events.append(("replace", str(dst)))
            real_replace(src, dst)

        def unlink(path: Any, *args: Any, **kwargs: Any) -> None:
            if not str(path).endswith(".part"):
                events.append(("unlink", str(path)))
            real_unlink(path, *args, **kwargs)

        def rmdir(path: Any, *args: Any, **kwargs: Any) -> None:
            events.append(("rmdir", str(path)))
            real_rmdir(path, *args, **kwargs)

        for name, fake in (("replace", replace), ("unlink", unlink), ("rmdir", rmdir)):
            attr = patch.object(pl.os, name, fake)
            attr.start()
            self.addCleanup(attr.stop)
        attr = patch.object(pl, "sync_directory", lambda directory: events.append(("sync", str(directory))))
        attr.start()
        self.addCleanup(attr.stop)
        return events

    def test_a_write_creates_and_syncs_directories_then_syncs_after_each_replace(self) -> None:
        events = self.record()
        entry = self.write()
        lib, root = str(self.library.directory), str(self.root)
        entry_path = str(self.library.path_of(entry.file))
        self.assertIn(("sync", root), events, "the new library directory is made durable in the root")
        replaced = events.index(("replace", entry_path))
        self.assertEqual(events[replaced + 1], ("sync", lib))
        index_replaced = events.index(("replace", str(self.library.directory / pl.INDEX_FILE)))
        self.assertEqual(events[index_replaced + 1], ("sync", lib))

    def test_a_confined_removal_syncs_its_directory_after_the_unlink(self) -> None:
        entry = self.write()
        events = self.record()
        path = self.library.path_of(entry.file)
        pl.remove_confined_file(path, self.root)
        self.assertEqual(events, [("unlink", str(path)), ("sync", str(self.library.directory))])

    def test_a_failed_directory_sync_is_an_error(self) -> None:
        with patch.object(pl, "sync_directory", side_effect=OSError("fsync failed")), self.assertRaises(OSError):
            pl.publish_bytes_atomically(self.root / "x.msprompt", b"x")

    @unittest.skipUnless(os.name == "posix", "directory fsync exists on POSIX only")
    def test_a_real_directory_sync_succeeds(self) -> None:
        pl.sync_directory(self.root)

    def test_directory_sync_is_a_no_op_off_posix(self) -> None:
        with patch.object(pl.os, "name", "nt"), patch.object(pl.os, "open", side_effect=AssertionError("opened a directory")):
            pl.sync_directory(self.root)


class RegistryTests(unittest.TestCase):
    def test_one_instance_per_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertIs(pl.library_for(root, SPEC), pl.library_for(root, SPEC))
            with self.assertRaises(ValueError):
                pl.library_for(root, pl.LibrarySpec(SPEC.library_id, "other", "bfloat16", "BF16"))
            self.assertIsNot(pl.library_for(root, SPEC), pl.library_for(root, pl.LibrarySpec("second", "prompt_embeds", "bfloat16", "BF16")))


class ConcurrencyTests(_LibraryCase):
    def test_parallel_writes_of_distinct_texts_all_land(self) -> None:
        errors: list[BaseException] = []

        def worker(index: int) -> None:
            try:
                self.library.write_entry(f"text {index}", _FakeTensor(), _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
            except BaseException as exc:  # noqa: BLE001 - reported below
                errors.append(exc)

        threads = [threading.Thread(target=worker, args=(index,)) for index in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(errors, [])
        self.assertEqual(len(self.library.reconcile().entries), 8)
        self.assertEqual(len(self.index()["entries"]), 8)


@unittest.skipUnless(all(importlib.util.find_spec(name) is not None for name in ("torch", "safetensors")), "needs torch and safetensors")
class RealSafetensorsTests(unittest.TestCase):
    def test_a_real_tensor_round_trips(self) -> None:
        import torch

        with tempfile.TemporaryDirectory() as tmp:
            library = pl.PromptLibrary(Path(tmp), SPEC)
            tensor = torch.arange(24, dtype=torch.float32).reshape(1, 3, 8).to(torch.bfloat16)
            entry, written = library.write_entry("real", tensor, _encoder(), max_sequence_length=512, name="", origin=pl.ORIGIN_BUILT)
            self.assertTrue(written)
            loaded = library.read_tensor(entry)
            self.assertTrue(torch.equal(loaded, tensor))
            renamed = library.rename_text("real", "renamed")[0]
            self.assertTrue(torch.equal(library.read_tensor(renamed), tensor))


if __name__ == "__main__":
    unittest.main()
