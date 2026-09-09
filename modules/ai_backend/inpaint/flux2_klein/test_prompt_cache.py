"""
File: modules/ai_backend/inpaint/flux2_klein/test_prompt_cache.py

Purpose:
Unit tests for `prompt_cache.py` and the service methods built on it: name
sanitization, the encoder fingerprint and the family name it feeds, the
directory layout of a save, and every refusal a `.msprompt` load owes the user.

Main responsibilities:
- verify nothing composes a path outside `prompt_cache/`;
- verify a listing survives a corrupt file;
- verify the refusals of a foreign encoder, another sequence length, another
  dtype, another fp8 flag, a foreign container and a newer version;
- verify the name collision rule and that an import is filed under the family
  recorded IN THE FILE;
- verify `build` encodes without building a pipeline and lets the encoder go;
- verify a cached prompt is enough to generate with no text encoder present.

Notes:
`PromptCacheRoundTripTests` needs REAL torch, because `safetensors.torch` is what
writes and reads the embedding tensor; it skips itself where torch is absent, and
everything else about the file is covered from its safetensors HEADER alone.
"""

from __future__ import annotations

import contextlib
import importlib.util
import json
import struct
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware, prompt_cache
from modules.ai_backend.runtime.model_manager import LoadedModelManager

from ._test_fixtures import _FakeEmbeds, _PlacementFixture, _TempTreeCase, _png_bytes


# ---------------------------------------------------------------------------
# The prompt-cache library
# ---------------------------------------------------------------------------
def _prompt_cache_bytes(metadata: dict[str, str], *, dtype_token: str = "BF16") -> bytes:
    """A syntactically valid `.msprompt` container carrying `metadata`.

    The tensor is a stub: everything the library layer does — listing,
    compatibility checking, filing an import under its own family — reads the
    HEADER only, and building a real tensor would drag torch into tests that are
    about directory layout. The one place that does load a tensor has its own
    torch-gated round-trip test below.
    """
    payload = b"\x00" * 64
    header = {
        "prompt_embeds": {"dtype": dtype_token, "shape": [1, 4, 8], "data_offsets": [0, 64]},
        "__metadata__": {str(key): str(value) for key, value in metadata.items()},
    }
    blob = json.dumps(header).encode("utf-8")
    return struct.pack("<Q", len(blob)) + blob + payload


@contextlib.contextmanager
def _patched_sizes(sizes: dict[str, int]):
    """Patch BOTH component sizers with the same figures, for one `with` block.

    `forecast_memory` asks `text_encoder_resident_bytes` for the encoder and
    `_weight_bytes` for the transformer and the VAE, so a test that patches only
    one leaves the encoder at the fixture tree's real (tiny) size and stops
    measuring what it says it measures.
    """
    with patch.object(components, "_weight_bytes", lambda path: sizes.get(path, 0)):
        with patch.object(
            components, "text_encoder_resident_bytes", lambda path, *_a, **_k: sizes.get(path, 0)
        ):
            yield


def _write_prompt_cache_file(path: Path, metadata: dict[str, str], **kwargs: object) -> None:
    """Write a stub `.msprompt` file, creating its family directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_prompt_cache_bytes(metadata, **kwargs))  # type: ignore[arg-type]


class NameSanitizationTests(unittest.TestCase):
    """A family or entry name is untrusted text and must stay one path component."""

    def test_separators_and_traversal_cannot_survive(self) -> None:
        for raw in ("../../etc/passwd", "a/b", "a\\b", "..", ".", "  ..  "):
            with self.subTest(raw=raw):
                try:
                    safe = svc.sanitize_name_component(raw, what="имя")
                except ValueError:
                    continue
                self.assertNotIn("/", safe)
                self.assertNotIn("\\", safe)
                self.assertNotEqual(safe, "..")
                self.assertEqual(Path(safe).name, safe)

    def test_an_empty_or_dots_only_name_is_refused(self) -> None:
        for raw in ("", "   ", "...", "///"):
            with self.subTest(raw=raw):
                with self.assertRaises(ValueError):
                    svc.sanitize_name_component(raw, what="имя кэша")

    def test_ordinary_names_are_kept_verbatim(self) -> None:
        self.assertEqual(
            svc.sanitize_name_component("Удаление текста (v2)", what="имя"),
            "Удаление текста (v2)",
        )

    def test_a_long_name_is_truncated(self) -> None:
        safe = svc.sanitize_name_component("x" * 500, what="имя")
        self.assertEqual(len(safe), svc._MAX_NAME_LENGTH)


class EncoderFingerprintTests(_TempTreeCase):
    """The identity a `.msprompt` file is checked against."""

    def _encoder(self) -> str:
        return self.paths["text_encoder_path"]

    def test_it_is_stable_across_calls(self) -> None:
        self.assertEqual(
            svc.text_encoder_fingerprint(self._encoder()),
            svc.text_encoder_fingerprint(self._encoder()),
        )

    def test_a_changed_config_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "config.json").write_text('{"a": 1}', encoding="utf-8")
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_changed_weight_size_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "model.safetensors").write_bytes(b"\x00" * 4096)
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_new_shard_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "model-2.safetensors").write_bytes(b"\x00" * 8)
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_file_inside_the_folder_identifies_the_folder(self) -> None:
        inside = str(self.root / "text_encoder" / "model.safetensors")
        self.assertEqual(
            svc.text_encoder_fingerprint(inside), svc.text_encoder_fingerprint(self._encoder())
        )

    def test_a_directory_without_a_config_cannot_be_identified(self) -> None:
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.text_encoder_fingerprint(self._encoder())

    def test_the_family_name_is_readable_and_unique(self) -> None:
        encoder_id = svc.text_encoder_fingerprint(self._encoder())
        family = svc.encoder_family_name(self._encoder(), encoder_id)
        self.assertTrue(family.startswith("text_encoder-"))
        self.assertTrue(family.endswith(encoder_id[: svc.PROMPT_CACHE_FAMILY_HASH_CHARS]))

    def test_two_encoders_with_the_same_directory_name_get_different_families(self) -> None:
        # The common case: every checkout calls its encoder folder `text_encoder`.
        other = self.root / "other" / "text_encoder"
        other.mkdir(parents=True)
        (other / "config.json").write_text('{"hidden": 4096}', encoding="utf-8")
        (other / "model.safetensors").write_bytes(b"\x00" * 64)
        mine = svc.encoder_family_name(
            self._encoder(), svc.text_encoder_fingerprint(self._encoder())
        )
        theirs = svc.encoder_family_name(str(other), svc.text_encoder_fingerprint(str(other)))
        self.assertNotEqual(mine, theirs)


class PromptCacheLibraryTests(_TempTreeCase):
    """`prompt_cache/<family>/<name>.msprompt`: layout, listing and the two copies.

    Serialization is stubbed (`_prompt_cache_bytes`) so these stay torch-free;
    what they pin is WHERE files land, which of them a listing accepts, and which
    of them a load refuses. The atomic publish itself is NOT stubbed.
    """

    def setUp(self) -> None:
        super().setUp()
        self.library = self.root / "program"
        self.library.mkdir()
        root_patch = patch.object(prompt_cache, "program_root", lambda: self.library)
        root_patch.start()
        self.addCleanup(root_patch.stop)

        def _write(dest: Path, _embeds: object, metadata: dict[str, str]) -> int:
            return svc.publish_bytes_atomically(dest, _prompt_cache_bytes(metadata))

        write_patch = patch.object(prompt_cache, "write_prompt_file", _write)
        write_patch.start()
        self.addCleanup(write_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.encoder_id = svc.text_encoder_fingerprint(self.paths["text_encoder_path"])
        self.family = svc.encoder_family_name(self.paths["text_encoder_path"], self.encoder_id)

    def _seed_cache(self, prompt: str = "remove the text", **overrides: object) -> dict[str, Any]:
        """Put a stand-in embedding in the LRU under the real key."""
        normalized = svc.normalize_flux2_klein_params(self.params(prompt=prompt, **overrides))
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(prompt)
        return normalized

    def _paths_without_encoder(self, **overrides: object) -> dict[str, object]:
        """`params()` with no text-encoder path: a machine that never downloaded one."""
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _foreign_metadata(self, **overrides: str) -> dict[str, str]:
        """Metadata of a file built by ANOTHER encoder."""
        normalized = svc.normalize_flux2_klein_params(self.params(prompt="foreign"))
        metadata = svc.prompt_file_metadata(normalized, "foreign", "f" * 64, "other-ffffffff")
        metadata.update(overrides)
        return metadata

    # ---- save ----
    def test_save_writes_into_the_family_directory(self) -> None:
        self._seed_cache()
        out = self.service.prompt_cache_save(self.params(prompt="remove the text"), "убрать текст")
        dest = Path(out["path"])
        self.assertTrue(dest.is_file())
        self.assertEqual(dest.parent, self.library / "prompt_cache" / self.family)
        self.assertEqual(out["family"], self.family)
        self.assertEqual(out["name"], "убрать текст")

    def test_save_without_a_cached_prompt_says_what_to_do(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(self.params(prompt="never encoded"), "x")
        message = str(caught.exception)
        self.assertIn("не закодирован", message)
        self.assertIn("build", message)
        # Nothing was written: a failed save must not leave a stub behind.
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_save_refuses_an_empty_prompt(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_save(self.params(prompt="   "), "x")

    def test_a_name_collision_is_refused_unless_overwrite_was_asked_for(self) -> None:
        self._seed_cache()
        params = self.params(prompt="remove the text")
        first = self.service.prompt_cache_save(params, "preset")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(params, "preset")
        self.assertIn("уже существует", str(caught.exception))
        again = self.service.prompt_cache_save(params, "preset", overwrite=True)
        self.assertEqual(first["path"], again["path"])

    def test_the_stored_name_is_the_sanitized_one(self) -> None:
        self._seed_cache()
        out = self.service.prompt_cache_save(self.params(prompt="remove the text"), "a/b")
        self.assertEqual(out["name"], "a_b")
        self.assertEqual(Path(out["path"]).parent.name, self.family)

    # ---- list ----
    def test_listing_reports_the_entries_of_this_family_only(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        _write_prompt_cache_file(
            self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt",
            self._foreign_metadata(),
        )
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual(out["family"], self.family)
        self.assertEqual([entry["name"] for entry in out["entries"]], ["mine"])
        self.assertEqual(out["entries"][0]["prompt"], "remove the text")
        self.assertEqual(out["skipped"], [])

    def test_a_corrupt_file_is_skipped_instead_of_failing_the_listing(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "good")
        broken = self.library / "prompt_cache" / self.family / "broken.msprompt"
        broken.write_bytes(b"not safetensors at all")
        foreign = self.library / "prompt_cache" / self.family / "foreign.msprompt"
        foreign.write_bytes(_prompt_cache_bytes({"format": "someone.else"}))

        out = self.service.prompt_cache_list(self.params())
        self.assertEqual([entry["name"] for entry in out["entries"]], ["good"])
        self.assertEqual(
            sorted(entry["name"] for entry in out["skipped"]), ["broken", "foreign"]
        )
        self.assertTrue(all(entry["reason"] for entry in out["skipped"]))

    def test_an_empty_library_lists_nothing_rather_than_failing(self) -> None:
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual(out["entries"], [])
        self.assertEqual(out["skipped"], [])

    def test_every_entry_names_the_family_it_sits_in(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual([entry["family"] for entry in out["entries"]], [self.family])
        self.assertTrue(out["text_encoder_available"])

    def test_listing_without_an_encoder_spans_every_family(self) -> None:
        # Two families, one of them ours; without an encoder neither is "current",
        # so both must be listed or the entries that make an encoder-less machine
        # usable would be invisible.
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        _write_prompt_cache_file(
            self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt",
            self._foreign_metadata(),
        )
        out = self.service.prompt_cache_list({})
        self.assertEqual(
            sorted((entry["family"], entry["name"]) for entry in out["entries"]),
            [("other-ffffffff", "theirs"), (self.family, "mine")],
        )
        # No family is active, and the directory is the library root.
        self.assertEqual(out["family"], "")
        self.assertEqual(out["directory"], str(self.library / "prompt_cache"))
        self.assertFalse(out["text_encoder_available"])

    def test_a_nonexistent_encoder_path_lists_like_an_absent_one(self) -> None:
        # What a settings file carried over from another machine looks like.
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        out = self.service.prompt_cache_list(
            self.params(text_encoder_path=str(self.root / "gone"))
        )
        self.assertEqual(out["family"], "")
        self.assertFalse(out["text_encoder_available"])
        self.assertEqual([entry["name"] for entry in out["entries"]], ["mine"])

    def test_a_corrupt_file_is_named_by_family_in_a_library_wide_listing(self) -> None:
        bad = self.library / "prompt_cache" / "other-ffffffff" / "broken.msprompt"
        bad.parent.mkdir(parents=True)
        bad.write_bytes(b"not safetensors at all")
        out = self.service.prompt_cache_list({})
        self.assertEqual(out["entries"], [])
        self.assertEqual(
            [(item["family"], item["name"]) for item in out["skipped"]],
            [("other-ffffffff", "broken")],
        )

    def test_saving_without_an_encoder_is_refused_with_the_reason(self) -> None:
        self._seed_cache()
        params = self.params(prompt="remove the text")
        params.pop("text_encoder_path")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(params, "mine")
        message = str(caught.exception)
        self.assertIn("энкодер", message)
        # Saving records WHICH encoder built the entry, so a ready cache is no
        # substitute here and must not be offered as one.
        self.assertNotIn("prompt_cache.load", message)
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_export_without_an_encoder_resolves_the_entry_across_families(self) -> None:
        self._seed_cache()
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        dest = self.root / "outside" / "shared.msprompt"
        dest.parent.mkdir()
        out = self.service.prompt_cache_export({}, "mine", str(dest))
        self.assertEqual(out["family"], self.family)
        self.assertEqual(dest.read_bytes(), Path(saved["path"]).read_bytes())

    # ---- load without a local encoder ----
    def test_an_ambiguous_name_is_refused_instead_of_guessed(self) -> None:
        for family in (self.family, "other-ffffffff"):
            _write_prompt_cache_file(
                self.library / "prompt_cache" / family / "twin.msprompt",
                self._foreign_metadata(),
            )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(prompt="x"), "twin")
        message = str(caught.exception)
        self.assertIn(self.family, message)
        self.assertIn("other-ffffffff", message)

    def test_a_missing_entry_without_an_encoder_names_the_library(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(), "absent")
        self.assertIn("absent", str(caught.exception))
        self.assertIn(str(self.library / "prompt_cache"), str(caught.exception))

    def test_without_an_encoder_the_settings_are_still_checked(self) -> None:
        # Everything except the fingerprint needs no encoder, so nothing except
        # the fingerprint may be waived: a file of another sequence length, dtype
        # or fp8 setting is still refused, and the tensor is never allocated.
        cases = {
            "max_sequence_length": {"max_sequence_length": "256"},
            "dtype": {"dtype": "float16"},
            "text_encoder_fp8": {"text_encoder_fp8": "true"},
        }
        for field, overrides in cases.items():
            with self.subTest(field=field):
                path = self.library / "prompt_cache" / "other-ffffffff" / f"{field}.msprompt"
                _write_prompt_cache_file(path, self._foreign_metadata(**overrides))
                with self.assertRaises(ValueError):
                    self.service.prompt_cache_load(self._paths_without_encoder(), field)

    def test_a_foreign_container_is_still_refused_without_an_encoder(self) -> None:
        path = self.library / "prompt_cache" / "other-ffffffff" / "alien.msprompt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(_prompt_cache_bytes({"format": "something.else"}))
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(), "alien")
        self.assertIn(svc.PROMPT_CACHE_FORMAT, str(caught.exception))

    # ---- load refusals (the successful path needs torch; see the round trip) ----
    def test_loading_a_missing_entry_names_it(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "absent")
        self.assertIn("absent", str(caught.exception))

    def test_an_entry_of_another_encoder_is_refused(self) -> None:
        # The file sits in OUR family directory (a user copied it there by hand),
        # so only the recorded fingerprint can tell it apart.
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "smuggled.msprompt",
            self._foreign_metadata(),
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "smuggled")
        self.assertIn("другим текстовым энкодером", str(caught.exception))

    def test_another_sequence_length_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(max_sequence_length=256))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "short.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(max_sequence_length=512), "short")
        self.assertIn("max_sequence_length", str(caught.exception))

    def test_an_entry_written_under_float16_is_refused_and_says_to_rebuild(self) -> None:
        # The state a user is left in by the encoder-dtype change: an entry saved
        # while the service still encoded at float16. Its embedding really did
        # come from a different encoder precision, so the refusal is correct —
        # but it has to name float16, name bfloat16, and say what to do.
        normalized = svc.normalize_flux2_klein_params(self.params())
        metadata = dict(
            svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        )
        metadata["dtype"] = "float16"
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "half.msprompt",
            metadata,
            dtype_token="F16",
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "half")
        message = str(caught.exception)
        self.assertIn("float16", message)
        self.assertIn("bfloat16", message)
        self.assertIn("заново", message)

    def test_the_request_dtype_no_longer_decides_what_an_entry_must_be(self) -> None:
        # `dtype` governs the transformer and the VAE; the encoder is bfloat16 in
        # every case, so an entry written under one request dtype loads under the
        # other.
        normalized = svc.normalize_flux2_klein_params(self.params(dtype="bfloat16"))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        self.assertEqual(metadata["dtype"], "bfloat16")
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "either.msprompt", metadata
        )
        out = self.service.prompt_cache_load(self.params(dtype="float16"), "either")
        self.assertEqual(out["dtype"], "bfloat16")

    def test_metadata_that_lies_about_the_tensor_dtype_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(dtype="bfloat16"))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "lying.msprompt",
            metadata,
            dtype_token="F16",
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(dtype="bfloat16"), "lying")
        self.assertIn("повреждён", str(caught.exception))

    def test_another_fp8_setting_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(text_encoder_fp8=True))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "quantized.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(text_encoder_fp8=False), "quantized")
        self.assertIn("fp8", str(caught.exception))

    def test_a_file_that_is_not_ours_is_refused_by_its_marker(self) -> None:
        path = self.library / "prompt_cache" / self.family / "alien.msprompt"
        _write_prompt_cache_file(path, {"format": "some.other.tool", "format_version": "1"})
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "alien")
        self.assertIn(svc.PROMPT_CACHE_FORMAT, str(caught.exception))

    def test_a_newer_format_version_is_refused_rather_than_guessed(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        metadata["format_version"] = str(svc.PROMPT_CACHE_VERSION + 1)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "future.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "future")
        self.assertIn("не поддерживается", str(caught.exception))

    # ---- export / import ----
    def test_export_copies_the_entry_byte_for_byte(self) -> None:
        self._seed_cache()
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        dest = self.root / "outside" / "shared.msprompt"
        dest.parent.mkdir()
        out = self.service.prompt_cache_export(self.params(), "mine", str(dest))
        self.assertEqual(out["size_bytes"], Path(saved["path"]).stat().st_size)
        self.assertEqual(dest.read_bytes(), Path(saved["path"]).read_bytes())

    def test_export_refuses_a_foreign_suffix(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_export(
                self.params(), "mine", str(self.root / "shared.txt")
            )
        self.assertIn(svc.PROMPT_CACHE_SUFFIX, str(caught.exception))

    def test_export_refuses_a_relative_path(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        with self.assertRaises(ValueError):
            self.service.prompt_cache_export(self.params(), "mine", "shared.msprompt")

    def test_import_files_a_foreign_entry_under_its_own_family(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())

        out = self.service.prompt_cache_import(self.params(), str(source))
        self.assertEqual(out["family"], "other-ffffffff")
        self.assertFalse(out["family_matches"])
        self.assertEqual(out["current_family"], self.family)
        self.assertEqual(
            Path(out["path"]), self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt"
        )
        # It is NOT visible to the current encoder, which is the point of filing
        # it under its own family rather than the selected one.
        self.assertEqual(self.service.prompt_cache_list(self.params())["entries"], [])

    def test_import_of_our_own_family_matches(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        normalized = svc.normalize_flux2_klein_params(self.params(prompt="mine"))
        metadata = svc.prompt_file_metadata(normalized, "mine", self.encoder_id, self.family)
        source = outside / "mine.msprompt"
        _write_prompt_cache_file(source, metadata)

        out = self.service.prompt_cache_import(self.params(), str(source), name="взято")
        self.assertTrue(out["family_matches"])
        self.assertEqual(out["name"], "взято")
        self.assertEqual(
            [entry["name"] for entry in self.service.prompt_cache_list(self.params())["entries"]],
            ["взято"],
        )

    def test_import_refuses_a_file_that_is_not_ours(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "alien.msprompt"
        _write_prompt_cache_file(source, {"format": "some.other.tool"})
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(source))
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_import_refuses_a_file_without_a_family(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        metadata = self._foreign_metadata()
        metadata.pop("text_encoder_family")
        source = outside / "orphan.msprompt"
        _write_prompt_cache_file(source, metadata)
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_import(self.params(), str(source))
        self.assertIn("text_encoder_family", str(caught.exception))

    def test_import_works_without_a_configured_encoder(self) -> None:
        # Setting a machine up from someone else's files: nothing is selected yet.
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())
        out = self.service.prompt_cache_import({}, str(source))
        self.assertEqual(out["current_family"], "")
        self.assertFalse(out["family_matches"])

    def test_import_honours_the_collision_rule(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())
        self.service.prompt_cache_import(self.params(), str(source))
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(source))
        self.service.prompt_cache_import(self.params(), str(source), overwrite=True)

    def test_import_refuses_a_missing_source(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(self.root / "nope.msprompt"))

    def test_a_family_name_from_a_file_cannot_escape_the_library(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        metadata = self._foreign_metadata(text_encoder_family="../../escaped")
        source = outside / "evil.msprompt"
        _write_prompt_cache_file(source, metadata)
        out = self.service.prompt_cache_import(self.params(), str(source))
        written = Path(out["path"]).resolve()
        self.assertTrue(written.is_relative_to((self.library / "prompt_cache").resolve()))


class PromptCacheRoundTripTests(_TempTreeCase):
    """The real serialization: a torch tensor out to disk and back.

    Gated on torch because `safetensors.torch` is what writes and reads the
    tensor; everything ABOUT the file — its metadata, its layout, its refusals —
    is covered torch-free above.
    """

    def setUp(self) -> None:
        super().setUp()
        if importlib.util.find_spec("torch") is None:  # pragma: no cover - host-dependent
            self.skipTest("torch is not installed")
        self.library = self.root / "program"
        self.library.mkdir()
        root_patch = patch.object(prompt_cache, "program_root", lambda: self.library)
        root_patch.start()
        self.addCleanup(root_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_a_saved_entry_loads_back_into_the_cache_unchanged(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="remove the text"))
        embeds = torch.arange(32, dtype=torch.bfloat16).reshape(1, 4, 8)
        key = self.service._prompt_cache_key(normalized, normalized["prompt"])
        self.service._prompt_cache[key] = embeds

        saved = self.service.prompt_cache_save(
            self.params(prompt="remove the text"), "круглый рейс"
        )
        self.assertGreater(saved["size_bytes"], 0)

        # A fresh service: nothing in memory, everything from the file.
        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        loaded = other.prompt_cache_load(self.params(prompt="ignored"), "круглый рейс")
        self.assertEqual(loaded["prompt"], "remove the text")
        self.assertTrue(loaded["prompt_cached"])
        restored = other._prompt_cache[key]
        self.assertEqual(restored.dtype, torch.bfloat16)
        self.assertTrue(torch.equal(restored, embeds))
        # The load answers for the prompt IN THE FILE, under the shared key.
        self.assertTrue(other._prompt_cached(self.params(prompt="remove the text")))

    def test_the_fingerprint_is_verified_when_the_encoder_is_there(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        loaded = self.service.prompt_cache_load(self.params(), "entry")
        self.assertTrue(loaded["encoder_verified"])

    def test_an_entry_loads_on_a_machine_that_has_no_encoder(self) -> None:
        # The scenario the format exists for: the `.msprompt` travels, the 16 GB
        # Qwen3 does not. The denoise and the VAE decode never look at the
        # encoder, so the only thing its absence costs is the fingerprint check —
        # and the answer says so instead of pretending it happened.
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="remove the text"))
        embeds = torch.arange(32, dtype=torch.bfloat16).reshape(1, 4, 8)
        key = self.service._prompt_cache_key(normalized, "remove the text")
        self.service._prompt_cache[key] = embeds
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "перенос")

        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        params = dict(self.params())
        params.pop("text_encoder_path")
        loaded = other.prompt_cache_load(params, "перенос")

        self.assertFalse(loaded["encoder_verified"])
        self.assertEqual(loaded["family"], Path(saved["path"]).parent.name)
        self.assertEqual(loaded["prompt"], "remove the text")
        # The embedding landed under the key a run on THIS machine looks up, so
        # the generation that follows needs no encoder either.
        self.assertTrue(other._prompt_cached({**params, "prompt": "remove the text"}))
        restored = other._prompt_cache[
            other._prompt_cache_key(
                svc.normalize_flux2_klein_params({**params, "prompt": "remove the text"}),
                "remove the text",
            )
        ]
        self.assertTrue(torch.equal(restored, embeds))

    def test_a_nonexistent_encoder_path_loads_the_same_way(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        loaded = other.prompt_cache_load(
            self.params(text_encoder_path=str(self.root / "no-such-encoder")), "entry"
        )
        self.assertFalse(loaded["encoder_verified"])

    def test_the_written_file_is_a_readable_container(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        saved = self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        metadata, tensor = svc.read_prompt_file_header(Path(saved["path"]))
        self.assertEqual(metadata["format"], svc.PROMPT_CACHE_FORMAT)
        self.assertEqual(metadata["prompt"], "p")
        self.assertEqual(tensor["dtype"], "BF16")
        self.assertEqual(tensor["shape"], [1, 2, 4])


class PromptCacheBuildTests(_PlacementFixture):
    """`prompt_cache.build`: encode the prompt, load nothing else, let go again."""

    def setUp(self) -> None:
        super().setUp()
        snapshot = {
            "ram_free": 64 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        memory_patch = patch.object(hardware, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def test_it_encodes_the_prompt_without_building_a_pipeline(self) -> None:
        out = self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertTrue(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])
        # The 9B transformer takes no part in a prompt.
        self.assertIsNone(self.service._pipe)
        self.assertNotIn("transformer", self.load_kwargs)
        self.assertNotIn("vae", self.load_kwargs)

    def test_it_releases_the_encoder_it_loaded(self) -> None:
        # The whole point of the button: cache the prompt so the 16 GB encoder
        # does not have to stay.
        self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertIsNone(self.service._text_encoder)

    def test_an_encoder_that_was_already_resident_is_left_alone(self) -> None:
        self._encode(prompt="first")  # a normal run keeps the encoder by default
        self.assertIsNotNone(self.service._text_encoder)
        self.service.prompt_cache_build(self.params(prompt="second"))
        self.assertIsNotNone(self.service._text_encoder)

    def test_a_second_build_of_the_same_prompt_reads_nothing(self) -> None:
        self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.load_kwargs.clear()
        out = self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertFalse(out["encoded"])
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_an_empty_prompt_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_build(self.params(prompt="   "))

    def test_the_status_flag_follows_the_shared_key(self) -> None:
        with patch.object(components, "is_torch_available", lambda: True):
            self.service.prompt_cache_build(self.params(prompt="a cat"))
            self.assertTrue(self.service.status(self.params(prompt="a cat"))["prompt_cached"])
            self.assertFalse(self.service.status(self.params(prompt="a dog"))["prompt_cached"])
            # Same prompt, another sequence length: a different embedding.
            self.assertFalse(
                self.service.status(self.params(prompt="a cat", max_sequence_length=256))[
                    "prompt_cached"
                ]
            )
            self.assertFalse(self.service.status(self.params(prompt="  "))["prompt_cached"])
            self.assertFalse(self.service.status(None)["prompt_cached"])

    def test_it_streams_the_prompt_phase_steps(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        self.service.prompt_cache_build(
            self.params(prompt="a cat"), progress_callback=lambda *frame: frames.append(frame)
        )
        self.assertTrue(all(phase == "load" for phase, _s, _t, _l in frames))
        steps = [step for _p, step, _t, _l in frames]
        self.assertIn(svc.LOAD_STEP_TEXT_ENCODER, steps)
        self.assertIn(svc.LOAD_STEP_ENCODE, steps)
        self.assertEqual(steps, sorted(steps))

    def test_the_memory_gate_charges_only_for_the_encoder(self) -> None:
        # A build must not demand the transformer's 18 GB of VRAM: it never
        # loads one. `encode_standalone` is what the guard checks.
        sizes = {
            self.paths["transformer_path"]: 18_157_185_168,
            self.paths["text_encoder_path"]: 16_381_516_808,
            self.paths["vae_path"]: 168_120_878,
        }
        with _patched_sizes(sizes):
            forecast = svc.forecast_memory(
                svc.normalize_flux2_klein_params(self.params()), 384, 384
            )
            phase = forecast["phases"]["encode_standalone"]
            self.assertEqual(phase["vram_bytes"], 0)
            self.assertLess(phase["ram_bytes"], forecast["phases"]["encode"]["ram_bytes"] + 1)
            self.assertGreaterEqual(phase["ram_bytes"], sizes[self.paths["text_encoder_path"]])
            # Adding the phase must not have moved the numbers the UI shows.
            self.assertEqual(
                forecast["vram_bytes"],
                max(
                    forecast["phases"][name]["vram_bytes"]
                    for name in ("encode", "denoise", "decode")
                ),
            )

    def test_a_host_short_of_memory_refuses_the_build_before_reading_anything(self) -> None:
        sizes = {self.paths["text_encoder_path"]: 16_381_516_808}
        snapshot = {
            "ram_free": 4 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        with _patched_sizes(sizes):
            with patch.object(hardware, "memory_snapshot", lambda _device=None: snapshot):
                with self.assertRaises(RuntimeError) as caught:
                    self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertIn("кэширование", str(caught.exception))
        self.assertNotIn("text_encoder", self.load_kwargs)


class NoTextEncoderTests(_PlacementFixture):
    """A machine where the 16 GB Qwen3 was never downloaded.

    The contract: a cached prompt generates (the denoise and the VAE decode never
    look at the encoder), and everything that would have to ENCODE is refused
    with a message naming both ways out.
    """

    def setUp(self) -> None:
        super().setUp()
        snapshot = {
            "ram_free": 64 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        memory_patch = patch.object(hardware, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def _params(self, **overrides: object) -> dict[str, object]:
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _cache(self, params: dict[str, object]) -> dict[str, Any]:
        normalized = svc.normalize_flux2_klein_params(params)
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(str(normalized["prompt"]))
        return normalized

    def test_an_absent_encoder_path_normalizes_instead_of_raising(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self._params(prompt="a cat"))
        self.assertEqual(normalized["text_encoder_path"], "")
        # The other two paths are still mandatory: nothing can replace them.
        for key in ("transformer_path", "vae_path"):
            with self.subTest(key=key):
                params = self._params()
                params.pop(key)
                with self.assertRaises(ValueError):
                    svc.normalize_flux2_klein_params(params)

    def test_a_path_that_is_not_on_disk_counts_as_no_encoder(self) -> None:
        params = self.params(text_encoder_path=str(self.root / "gone"))
        self.assertFalse(svc.text_encoder_available(svc.normalize_flux2_klein_params(params)))
        self.assertIsNone(svc.local_encoder_identity(str(self.root / "gone")))
        configured = svc.normalize_flux2_klein_params(self.params())
        self.assertTrue(svc.text_encoder_available(configured))

    def test_an_encoder_that_exists_but_cannot_be_identified_is_still_an_error(self) -> None:
        # Degrading a broken checkout into "no encoder" would hide it behind a
        # silently weaker check; it stays the error it always was.
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.local_encoder_identity(self.paths["text_encoder_path"])

    def test_a_cached_prompt_is_served_without_touching_the_encoder(self) -> None:
        normalized = self._cache(self._params(prompt="a cat"))
        embeds = self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        self.assertEqual(embeds["prompt"].text, "a cat")
        self.assertIsNone(embeds["negative"])
        self.assertNotIn("text_encoder", self.load_kwargs)
        self.assertEqual(self.encode_calls, [])

    def test_an_uncached_prompt_is_refused_naming_both_ways_out(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self._params(prompt="a cat"))
        with self.assertRaises(ValueError) as caught:
            self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        message = str(caught.exception)
        self.assertIn("энкодер", message)
        self.assertIn("кэш", message)
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_the_refusal_names_a_configured_path_that_is_missing(self) -> None:
        missing = str(self.root / "gone")
        normalized = svc.normalize_flux2_klein_params(
            self.params(prompt="a cat", text_encoder_path=missing)
        )
        with self.assertRaises(ValueError) as caught:
            self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        self.assertIn(missing, str(caught.exception))

    def test_a_run_with_an_uncached_prompt_loads_nothing_at_all(self) -> None:
        # The refusal has to happen BEFORE the 18 GB transformer is read: loading
        # a pipeline for a run that cannot finish is the cost this check avoids.
        region = np.full((128, 128, 3), 90, dtype=np.uint8)
        mask = np.zeros((128, 128), dtype=np.uint8)
        mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(region, "RGB"),
                _png_bytes(mask, "L"),
                params=self._params(prompt="a cat"),
            )
        self.assertEqual(self.load_kwargs, {})
        self.assertIsNone(self.service._pipe)

    def test_a_run_with_a_cached_prompt_gets_past_the_check(self) -> None:
        # Only the encoder gate is under test here, so the run is stopped right
        # after it by a pipeline build that refuses to happen — what matters is
        # WHICH error comes back, and that it is no longer the encoder one.
        self._cache(self._params(prompt="a cat"))
        region = np.full((128, 128, 3), 90, dtype=np.uint8)
        mask = np.zeros((128, 128), dtype=np.uint8)
        mask[48:80, 48:80] = 255
        def _reached(*_args: object, **_kwargs: object) -> object:
            raise RuntimeError("pipeline reached")

        with patch.object(self.service, "_ensure_pipeline_locked", _reached):
            with self.assertRaises(RuntimeError) as caught:
                self.service.inpaint_image_bytes(
                    _png_bytes(region, "RGB"),
                    _png_bytes(mask, "L"),
                    params=self._params(prompt="a cat"),
                )
        self.assertEqual(str(caught.exception), "pipeline reached")

    def test_build_without_an_encoder_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertIn("энкодер", str(caught.exception))
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_build_of_an_already_cached_prompt_needs_no_encoder(self) -> None:
        # Nothing has to be encoded, so nothing has to be refused.
        self._cache(self._params(prompt="a cat"))
        out = self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertFalse(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertNotIn("text_encoder", self.load_kwargs)

if __name__ == "__main__":
    unittest.main()
