"""
File: modules/ai_backend/inpaint/flux2_klein/test_prompt_cache.py

Purpose:
Unit tests for FLUX.2 klein's prompt library (`prompt_cache.py` over
`runtime/prompt_library.py`) and the service methods built on it.

Main responsibilities:
- pin the encoder identity: the fingerprint formula, its RoPE salt (and the
  pre-fix identity `rope_salt=False`), GGUF identities, literal characterization
  values;
- the selection end to end through `.status` (`prompt_cached`,
  `prompt_cache_entry`): preferred source, precision, the other source as a
  fallback, the guidance `""` text, the transformer width filter;
- `.prompt_cache.build` (named presets, rename without encoding, reuse of an
  in-memory embedding, idempotence), `.list`, `.export`, `.import` (v2 and v1);
- the v1 migration table (migrated / deleted / kept), its verification order,
  the removal of emptied family directories and its idempotence; its
  containment (a linked root, family or file and a `..` path are never
  followed or deleted) and durability order (directory fsync before the v1
  unlink, fault injection);
- a run that serves every text from the library needs no encoder; a vanished
  file re-plans;
- a machine with no encoder.

Notes:
- Every test runs with `prompt_cache.program_root` patched to a temp directory
  (`_TempTreeCase`); nothing touches the real `prompt_cache/`.
- `_LibraryCase` stubs the tensor (de)serialization (`prompt_library.serialize_tensor`
  / `load_tensor`) so it stays torch-free; the `_ManagedRunFixture` classes
  use real torch tensors and real safetensors files.
"""

from __future__ import annotations

import importlib.util
import json
import os
import struct
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware, prompt_cache
from modules.ai_backend.runtime import prompt_library as pl
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.safetensors_stream import read_safetensors_header

from ._test_fixtures import TINY_GGUF_ENCODER_CONFIG, _ManagedRunFixture, _TempTreeCase, _png_bytes, _write_qwen3_gguf_encoder, load_9b_config, seed_memory

LIB_9B = "flux2_klein_9b"
LIB_4B = "flux2_klein_4b"
ORDER = ["bf16", "q8_0", "fp8", "q6_k", "q5_k_m", "q4_k_m", "gguf"]


# ---------------------------------------------------------------------------
# Stdlib containers
# ---------------------------------------------------------------------------
class _StubTensor:
    """A tensor stand-in for the torch-free tests: a shape and its bytes (2 per element)."""

    def __init__(self, shape: tuple[int, ...] = (1, 4, 8), fill: int = 3) -> None:
        self.shape = tuple(shape)
        count = 1
        for dim in self.shape:
            count *= dim
        self.data = bytes([fill]) * (2 * count)

    def detach(self) -> "_StubTensor":
        return self

    def to(self, *_args: object, **_kwargs: object) -> "_StubTensor":
        return self


def _container(metadata: dict[str, str], *, shape: list[int] | None = None, dtype_token: str = "BF16", fill: int = 5) -> bytes:
    """A real (stdlib-built) safetensors container of one `prompt_embeds` tensor."""
    tensor = _StubTensor(tuple(shape or [1, 4, 8]), fill)
    header = {"prompt_embeds": {"dtype": dtype_token, "shape": list(tensor.shape), "data_offsets": [0, len(tensor.data)]}, "__metadata__": {str(k): str(v) for k, v in metadata.items()}}
    blob = json.dumps(header).encode("utf-8")
    blob += b" " * (-(8 + len(blob)) % 8)
    return struct.pack("<Q", len(blob)) + blob + tensor.data


def _write_prompt_cache_file(path: Path, metadata: dict[str, str], **kwargs: Any) -> None:
    """Write a stdlib `.msprompt` container, creating its directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_container(metadata, **kwargs))


def _stub_serialize(tensor_name: str, tensor: _StubTensor, metadata: dict[str, str]) -> bytes:
    header = {tensor_name: {"dtype": "BF16", "shape": list(tensor.shape), "data_offsets": [0, len(tensor.data)]}, "__metadata__": dict(metadata)}
    blob = json.dumps(header).encode("utf-8")
    blob += b" " * (-(8 + len(blob)) % 8)
    return struct.pack("<Q", len(blob)) + blob + tensor.data


def _stub_load(path: Path, tensor_name: str) -> _StubTensor:
    return _StubTensor(tuple(read_safetensors_header(path)[tensor_name]["shape"]))


def _v1_metadata(**overrides: str) -> dict[str, str]:
    """A protocol-22 (v1) entry's metadata; the path is fictional."""
    metadata = {
        "format": prompt_cache.V1_FORMAT,
        "format_version": "1",
        "prompt": "remove the text",
        "max_sequence_length": "512",
        "dtype": "bfloat16",
        "text_encoder_fp8": "false",
        "text_encoder_id": "f" * 64,
        "text_encoder_family": "qwen-te-ffffffff",
        "text_encoder_path": "/home/u/models/qwen-te/",
        "created_at": "2026-09-02T17:28:47Z",
    }
    metadata.update(overrides)
    return metadata


# ---------------------------------------------------------------------------
# Encoder identity
# ---------------------------------------------------------------------------
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


class EncoderRopeSaltTests(_TempTreeCase):
    """The fingerprint carries the RoPE normalization, so caches made under the wrong base are never served (D17)."""

    def _encoder(self, config: dict[str, object]) -> str:
        folder = self.root / "text_encoder"
        (folder / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
        return str(folder)

    def _unsalted(self, folder: str) -> str:
        """The fingerprint formula WITHOUT any salt: config bytes, then each weight file's name and size."""
        import hashlib

        source = Path(folder)
        digest = hashlib.sha256((source / "config.json").read_bytes())
        for entry in sorted((item for item in source.iterdir() if item.suffix == ".safetensors"), key=lambda item: item.name):
            digest.update(f"\n{entry.name}\x00{entry.stat().st_size}".encode("utf-8"))
        return digest.hexdigest()

    def test_a_normalized_config_is_salted_with_its_base(self) -> None:
        import hashlib

        folder = self._encoder(load_9b_config("uncensored_text_encoder"))
        fingerprint = svc.text_encoder_fingerprint(folder)
        self.assertNotEqual(fingerprint, self._unsalted(folder), "the pre-fix identity is never produced again")
        source = Path(folder)
        digest = hashlib.sha256((source / "config.json").read_bytes())
        digest.update(f"\nmodel.safetensors\x00{(source / 'model.safetensors').stat().st_size}".encode("utf-8"))
        digest.update(b"\nrope_theta=1000000.0")
        self.assertEqual(fingerprint, digest.hexdigest())

    def test_it_differs_from_the_same_config_without_rope_parameters(self) -> None:
        config = load_9b_config("uncensored_text_encoder")
        salted = svc.text_encoder_fingerprint(self._encoder(config))
        plain = svc.text_encoder_fingerprint(self._encoder({key: value for key, value in config.items() if key != "rope_parameters"}))
        self.assertNotEqual(salted, plain)

    def test_a_stock_config_keeps_its_identity(self) -> None:
        folder = self._encoder(load_9b_config("text_encoder"))
        self.assertEqual(svc.text_encoder_fingerprint(folder), self._unsalted(folder))
        self.assertEqual(svc.text_encoder_fingerprint(folder, rope_salt=False), self._unsalted(folder))

    def test_a_refused_rope_cannot_be_identified(self) -> None:
        folder = self._encoder({"rope_parameters": {"rope_type": "yarn", "rope_theta": 1000000}})
        with self.assertRaisesRegex(ValueError, "yarn"):
            svc.text_encoder_fingerprint(folder)


@unittest.skipUnless(all(importlib.util.find_spec(name) is not None for name in ("gguf", "torch", "transformers", "accelerate")), "needs gguf, torch, transformers and accelerate")
class GgufEncoderIdentityTests(_TempTreeCase):
    """A GGUF encoder's identity and family never coincide with its bf16 counterpart's, and the in-memory key carries the format."""

    def setUp(self) -> None:
        super().setUp()
        (self.root / "text_encoder" / "config.json").write_text(json.dumps(TINY_GGUF_ENCODER_CONFIG), encoding="utf-8")
        self.q8 = self.root / "text_encoder_gguf" / "Qwen3-tiny-Q8_0.gguf"
        _write_qwen3_gguf_encoder(self.q8)
        self.q4 = self.root / "text_encoder_gguf" / "Qwen3-tiny-Q4_0.gguf"
        _write_qwen3_gguf_encoder(self.q4, quant="Q4_0")

    def test_fingerprint_differs_from_bf16_and_per_file(self) -> None:
        folder = svc.text_encoder_fingerprint(self.paths["text_encoder_path"])
        q8 = svc.text_encoder_fingerprint(str(self.q8))
        q4 = svc.text_encoder_fingerprint(str(self.q4))
        self.assertEqual(len({folder, q8, q4}), 3)
        self.assertEqual(q8, svc.text_encoder_fingerprint(str(self.q8)), "stable")
        self.assertEqual(svc.encoder_label(str(self.q8)), "Qwen3-tiny-Q8_0.gguf", "named by the file the user picked")

    def test_a_gguf_whose_config_is_normalized_is_salted(self) -> None:
        import hashlib

        config = self.root / "text_encoder" / "config.json"
        config.write_text(json.dumps({**TINY_GGUF_ENCODER_CONFIG, "rope_parameters": {"rope_type": "default", "rope_theta": 1000000}}), encoding="utf-8")
        digest = hashlib.sha256(config.read_bytes())
        digest.update(f"\ngguf\x00{self.q8.name}\x00{self.q8.stat().st_size}".encode("utf-8"))
        unsalted = digest.hexdigest()
        digest.update(b"\nrope_theta=1000000.0")
        fingerprint = svc.text_encoder_fingerprint(str(self.q8))
        self.assertEqual(fingerprint, digest.hexdigest())
        self.assertNotEqual(fingerprint, unsalted)

    def test_a_gguf_without_a_config_cannot_be_identified(self) -> None:
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.text_encoder_fingerprint(str(self.q8))

    def test_the_provenance_carries_the_format_and_identity(self) -> None:
        formats = [{"id": "bf16", "path": self.paths["text_encoder_path"]}, {"id": "q8_0", "path": str(self.q8)}]
        bf16 = svc.encoder_provenance(svc.normalize_flux2_klein_params(self.params(text_encoder_format="bf16", text_encoder_formats=formats, text_encoder_source="stock")))
        q8 = svc.encoder_provenance(svc.normalize_flux2_klein_params(self.params(text_encoder_path=str(self.q8), text_encoder_format="q8_0", text_encoder_formats=formats, text_encoder_source="stock")))
        self.assertNotEqual(bf16.encoder_id, q8.encoder_id)
        self.assertEqual((q8.format, q8.source, q8.precision_id()), ("q8_0", "stock", "q8_0"))
        self.assertEqual(bf16.format, "bf16")
        self.assertEqual(pl.entry_file_name("p", 512, q8) == pl.entry_file_name("p", 512, bf16), False)


#: A recorded-header replica of a v1 entry made by the uncensored 9B encoder before
#: its RoPE fix (protocol <= 22): every field as written then, the path fictional.
_V1_UNCENSORED_REPLICA: dict[str, str] = {
    "format": "manhwastudio.flux2_klein.prompt_cache",
    "format_version": "1",
    "prompt": "Remove any text and sound effects, and restore the background underneath them",
    "max_sequence_length": "512",
    "dtype": "bfloat16",
    "text_encoder_fp8": "false",
    "text_encoder_id": "e52fb293409839842c9e2df378d0f10dc08aa2b1887e2edd30986e8812e991ca",
    "text_encoder_family": "flux2-klein-9b-uncensored-text-encoder-e52fb293",
    "text_encoder_path": "/home/u/sd_models/flux2-klein-9b-uncensored-text-encoder/",
    "created_at": "2026-09-02T17:28:47Z",
}


class FingerprintCharacterizationTests(_TempTreeCase):
    """Literal fingerprints pinned before the prompt library moved to protocol 23: identities must survive any refactor.

    A changed value would orphan every entry ever written (its recorded id would
    no longer match its encoder), so these are byte-for-byte pins, not formulas.
    """

    def _folder(self, config: dict[str, object]) -> str:
        folder = self.root / "text_encoder"
        (folder / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
        return str(folder)

    def test_a_v5_config_folder_has_its_salted_identity(self) -> None:
        self.assertEqual(svc.text_encoder_fingerprint(self._folder(load_9b_config("uncensored_text_encoder"))), "79fa5ee613ebea6964713d2dc760b91786017a3df1dbaa3ac7d6b208d6f492d3")

    def test_the_unsalted_identity_of_the_same_folder(self) -> None:
        import hashlib

        folder = Path(self._folder(load_9b_config("uncensored_text_encoder")))
        digest = hashlib.sha256((folder / "config.json").read_bytes())
        digest.update(f"\nmodel.safetensors\x00{(folder / 'model.safetensors').stat().st_size}".encode("utf-8"))
        self.assertEqual(digest.hexdigest(), "b74369eb07703465352f7c8d7d10b51e975a084f9aa88e7d94e797e7bd55f5a8")
        self.assertEqual(svc.text_encoder_fingerprint(str(folder), rope_salt=False), digest.hexdigest(), "the pre-fix identity the migration recognizes")

    def test_a_stock_config_folder_and_an_empty_config(self) -> None:
        self.assertEqual(svc.text_encoder_fingerprint(self._folder(load_9b_config("text_encoder"))), "9bf42aebcf5de7c1f28ac8611c677dbbde119903d9465615dc09f92947f0dc29")
        (self.root / "text_encoder" / "config.json").write_text("{}", encoding="utf-8")
        self.assertEqual(svc.text_encoder_fingerprint(self.paths["text_encoder_path"]), "2687cc43ed207f72210440694705e972c06435aa1960c79723729f42cc32f96f")

    @unittest.skipUnless(all(importlib.util.find_spec(name) is not None for name in ("gguf", "torch")), "needs gguf and torch")
    def test_a_gguf_encoder_file(self) -> None:
        (self.root / "text_encoder" / "config.json").write_text(json.dumps(TINY_GGUF_ENCODER_CONFIG), encoding="utf-8")
        gguf_file = self.root / "text_encoder_gguf" / "Qwen3-tiny-Q8_0.gguf"
        _write_qwen3_gguf_encoder(gguf_file)
        self.assertEqual(gguf_file.stat().st_size, 835232)
        self.assertEqual(svc.text_encoder_fingerprint(str(gguf_file)), "ccd1e813e071ad362ce6ff0f77f50a4422869d4d98a86e97a1049e0c46b1b224")

    def test_the_v1_header_of_a_recorded_entry_parses(self) -> None:
        path = self.root / "lib" / "flux2-klein-9b-uncensored-text-encoder-e52fb293" / "base.msprompt"
        _write_prompt_cache_file(path, _V1_UNCENSORED_REPLICA)
        metadata, tensor = prompt_cache.read_v1_header(path)
        self.assertEqual(metadata, _V1_UNCENSORED_REPLICA)
        self.assertEqual(tensor["dtype"], "BF16")


# ---------------------------------------------------------------------------
# The library through the service (torch-free)
# ---------------------------------------------------------------------------
class _LibraryCase(_TempTreeCase):
    """A service over the temp program root, with the tensor (de)serialization stubbed."""

    def setUp(self) -> None:
        super().setUp()
        for name, replacement in (("serialize_tensor", _stub_serialize), ("load_tensor", _stub_load)):
            attr = patch.object(pl, name, replacement)
            attr.start()
            self.addCleanup(attr.stop)
        self.service = svc.Flux2KleinInpaintService(SmallModelManager())

    def lib_params(self, **overrides: object) -> dict[str, object]:
        """`params()` naming the 9B library and the Rust precision order."""
        return self.params(**{"prompt_library": LIB_9B, "prompt_cache_precision_order": list(ORDER), **overrides})

    def without_encoder(self, **overrides: object) -> dict[str, object]:
        params = self.lib_params(**overrides)
        params.pop("text_encoder_path")
        return params

    def current(self, **overrides: object) -> pl.EncoderProvenance:
        """The provenance of the tree's encoder for `lib_params(**overrides)`."""
        provenance = prompt_cache.encoder_provenance(svc.normalize_flux2_klein_params(self.lib_params(**overrides)))
        assert provenance is not None
        return provenance

    def add_entry(
        self,
        prompt: str = "a cat",
        *,
        source: str = "stock",
        fmt: str = "bf16",
        fp8: bool = False,
        encoder_id: str | None = None,
        msl: int = 512,
        width: int = 8,
        name: str = "",
        library_id: str = LIB_9B,
        created_at: str = "2026-10-01T00:00:00Z",
    ) -> pl.LibraryEntry:
        """Write one stub entry straight into a library."""
        import hashlib

        encoder = pl.EncoderProvenance(source, fmt, fp8, encoder_id or hashlib.sha256(f"{source}/{fmt}".encode()).hexdigest(), f"{source}-{fmt}")
        with patch.object(pl, "utc_now", return_value=created_at):
            entry, written = prompt_cache.library_of(library_id).write_entry(prompt, _StubTensor((1, msl, width)), encoder, max_sequence_length=msl, name=name, origin=pl.ORIGIN_BUILT)
        self.assertTrue(written)
        return entry

    def seed_current(self, prompt: str = "a cat", **overrides: object) -> None:
        """An in-memory embedding of `prompt` made by the tree's encoder (what a run's encode leaves behind)."""
        params = self.lib_params(prompt=prompt, **overrides)
        msl = int(svc.normalize_flux2_klein_params(params)["max_sequence_length"])
        seed_memory(self.service, params, tensor=_StubTensor((1, msl, 8)), encoder=self.current(**overrides))


class StatusSelectionTests(_LibraryCase):
    """`.status` answers from `_prompt_plan`: which entry would serve the prompt, and whether every needed text is served."""

    def test_the_selected_source_wins_over_a_more_precise_entry_of_the_other(self) -> None:
        self.add_entry(source="uncensored", fmt="q8_0")
        self.add_entry(source="stock", fmt="bf16")
        out = self.service.status(self.lib_params(prompt="a cat", text_encoder_source="uncensored"))
        self.assertTrue(out["prompt_cached"])
        self.assertEqual((out["prompt_cache_entry"]["encoder_source"], out["prompt_cache_entry"]["encoder_format"]), ("uncensored", "q8_0"))
        out = self.service.status(self.lib_params(prompt="a cat", text_encoder_source="stock"))
        self.assertEqual((out["prompt_cache_entry"]["encoder_source"], out["prompt_cache_entry"]["encoder_format"]), ("stock", "bf16"))

    def test_the_other_source_serves_when_it_is_all_there_is_and_is_named(self) -> None:
        entry = self.add_entry(source="stock", fmt="q4_k_m", name="Кот")
        out = self.service.status(self.lib_params(prompt="a cat", text_encoder_source="uncensored"))
        self.assertTrue(out["prompt_cached"])
        info = out["prompt_cache_entry"]
        self.assertEqual((info["file"], info["name"], info["encoder_source"], info["encoder_format"], info["origin"]), (entry.file, "Кот", "stock", "q4_k_m", "built"))
        self.assertEqual(set(info), {"file", "name", "encoder_source", "encoder_format", "encoder_fp8", "encoder_label", "current_encoder", "created_at", "size_bytes", "origin"})

    def test_a_library_entry_needs_no_encoder(self) -> None:
        self.add_entry()
        out = self.service.status(self.without_encoder(prompt="a cat"))
        self.assertTrue(out["prompt_cached"])
        self.assertTrue(out["available"])
        self.assertFalse(out["text_encoder_available"])
        self.assertFalse(out["prompt_cache_entry"]["current_encoder"])

    def test_current_encoder_is_fingerprint_and_fp8(self) -> None:
        current = self.current()
        self.add_entry(source="stock", encoder_id=current.encoder_id)
        self.assertTrue(self.service.status(self.lib_params(prompt="a cat"))["prompt_cache_entry"]["current_encoder"])
        self.assertFalse(self.service.status(self.lib_params(prompt="a cat", text_encoder_fp8=True))["prompt_cache_entry"]["current_encoder"])

    def test_only_the_exact_text_and_length_match(self) -> None:
        self.add_entry("a cat")
        self.assertTrue(self.service.status(self.lib_params(prompt="  a cat \n"))["prompt_cached"], "both ends are stripped, as the run strips them")
        for prompt, msl in (("A cat", 512), ("a  cat", 512), ("a cat", 256)):
            with self.subTest(prompt=prompt, msl=msl):
                out = self.service.status(self.lib_params(prompt=prompt, max_sequence_length=msl))
                self.assertFalse(out["prompt_cached"])
                self.assertIsNone(out["prompt_cache_entry"])

    def test_under_guidance_the_empty_text_is_needed_too(self) -> None:
        self.declare_distilled(False)
        self.add_entry("a cat")
        params = self.lib_params(prompt="a cat", guidance_scale=4.0)
        out = self.service.status(params)
        self.assertFalse(out["prompt_cached"], "the run would still encode the negative prompt")
        self.assertIsNotNone(out["prompt_cache_entry"])
        self.assertEqual(self.service._prompt_plan(svc.normalize_flux2_klein_params(params), blocking_load=True).to_encode, ("",))
        self.add_entry("")
        self.assertTrue(self.service.status(params)["prompt_cached"])
        self.assertTrue(self.service.status(self.lib_params(prompt="a cat"))["prompt_cached"], "a distilled checkpoint needs no negative")

    def test_a_width_the_transformer_does_not_take_is_ignored(self) -> None:
        (self.root / "transformer" / "config.json").write_text(json.dumps({"joint_attention_dim": 24}), encoding="utf-8")
        self.add_entry(width=8)
        self.assertFalse(self.service.status(self.lib_params(prompt="a cat"))["prompt_cached"])
        self.add_entry(source="uncensored", width=24)
        out = self.service.status(self.lib_params(prompt="a cat", text_encoder_source="stock"))
        self.assertTrue(out["prompt_cached"])
        self.assertEqual(out["prompt_cache_entry"]["encoder_source"], "uncensored")

    def test_memory_ranks_after_precision_but_before_a_file(self) -> None:
        seed_memory(self.service, self.lib_params(prompt="a cat"), tensor=_StubTensor((1, 512, 8)), encoder=pl.EncoderProvenance("stock", "q8_0", False, "m" * 64, "mem"))
        out = self.service.status(self.lib_params(prompt="a cat", text_encoder_source="stock"))
        self.assertEqual((out["prompt_cache_entry"]["origin"], out["prompt_cache_entry"]["file"]), ("memory", ""))
        self.add_entry(source="stock", fmt="q8_0", encoder_id="m" * 64)
        self.assertEqual(self.service.status(self.lib_params(prompt="a cat", text_encoder_source="stock"))["prompt_cache_entry"]["origin"], "memory")
        self.add_entry(source="stock", fmt="bf16")
        self.assertEqual(self.service.status(self.lib_params(prompt="a cat", text_encoder_source="stock"))["prompt_cache_entry"]["encoder_format"], "bf16")

    def test_without_a_library_only_memory_of_no_library_serves(self) -> None:
        self.add_entry()
        params = self.params(prompt="a cat")
        self.assertFalse(self.service.status(params)["prompt_cached"])
        seed_memory(self.service, params)
        self.assertTrue(self.service.status(params)["prompt_cached"])
        self.assertFalse((self.program_root / "prompt_cache" / "flux2_klein_4b").exists())

    def test_status_never_migrates(self) -> None:
        legacy = self.program_root / "prompt_cache" / "qwen-te-ffffffff" / "x.msprompt"
        _write_prompt_cache_file(legacy, _v1_metadata(), shape=[1, 1, 12288])
        self.service.status(self.lib_params(prompt="remove the text"))
        self.assertTrue(legacy.exists())
        self.assertIsNone(prompt_cache.migration_report())


class BuildAndListTests(_LibraryCase):
    """`.prompt_cache.build` (named presets) and `.prompt_cache.list`."""

    def test_build_writes_a_named_preset_from_memory_without_encoding(self) -> None:
        self.seed_current("a cat sitting on a mat and looking out of the window at the birds")
        out = self.service.prompt_cache_build(self.lib_params(prompt="a cat sitting on a mat and looking out of the window at the birds"))
        self.assertFalse(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertFalse(out["renamed"])
        self.assertEqual(out["name"], pl.default_preset_name("a cat sitting on a mat and looking out of the window at the birds"))
        self.assertEqual(len(out["written"]), 1)
        written = out["written"][0]
        self.assertEqual((written["origin"], written["current_encoder"], written["name"]), ("built", True, out["name"]))
        listing = self.service.prompt_cache_list(self.lib_params())
        self.assertEqual([row["prompt"] for row in listing["texts"]], ["a cat sitting on a mat and looking out of the window at the birds"])
        self.assertEqual(listing["texts"][0]["name"], out["name"])
        self.assertEqual(listing["texts"][0]["selected"]["file"], written["file"])
        header = read_safetensors_header(prompt_cache.library_of(LIB_9B).path_of(written["file"]))
        self.assertEqual(header["__metadata__"]["text_encoder_path"], self.paths["text_encoder_path"])

    def test_a_repeated_build_writes_nothing_and_a_new_name_renames(self) -> None:
        self.seed_current()
        first = self.service.prompt_cache_build(self.lib_params(prompt="a cat"), name="Кот")
        self.assertEqual(first["name"], "Кот")
        again = self.service.prompt_cache_build(self.lib_params(prompt="a cat"))
        self.assertEqual((again["written"], again["encoded"], again["renamed"], again["name"]), ([], False, False, "Кот"))
        renamed = self.service.prompt_cache_build(self.lib_params(prompt="a cat"), name="  Котик\n")
        self.assertEqual((renamed["written"], renamed["renamed"], renamed["name"]), ([], True, "Котик"))
        self.assertEqual(self.service.prompt_cache_list(self.lib_params())["texts"][0]["name"], "Котик")

    def test_a_rename_reaches_every_entry_of_the_text(self) -> None:
        other = self.add_entry("a cat", source="uncensored", fmt="q8_0", name="old")
        self.seed_current()
        self.service.prompt_cache_build(self.lib_params(prompt="a cat"), name="new")
        library = prompt_cache.library_of(LIB_9B)
        self.assertEqual({entry.name for entry in library.snapshot().entries_of_text("a cat")}, {"new"})
        self.assertEqual(library.snapshot().entries[other.file].name, "new")

    def test_without_a_name_a_new_entry_joins_the_existing_preset_name(self) -> None:
        self.add_entry("a cat", source="uncensored", name="Существующий")
        self.seed_current()
        out = self.service.prompt_cache_build(self.lib_params(prompt="a cat"))
        self.assertEqual((out["name"], out["written"][0]["name"]), ("Существующий", "Существующий"))

    def test_build_refusals(self) -> None:
        with self.assertRaisesRegex(ValueError, "prompt_library"):
            self.service.prompt_cache_build(self.params(prompt="a cat"))
        with self.assertRaisesRegex(ValueError, "пуст"):
            self.service.prompt_cache_build(self.lib_params(prompt="  "))
        with self.assertRaisesRegex(ValueError, "энкодер"):
            self.service.prompt_cache_build(self.without_encoder(prompt="a cat"))
        with self.assertRaisesRegex(ValueError, "prompt_library"):
            self.service.prompt_cache_build(self.lib_params(prompt="a cat", prompt_library="Bad-Id"))

    def test_the_listing(self) -> None:
        self.add_entry("older", name="Old", created_at="2026-01-01T00:00:00Z")
        self.add_entry("newer", source="stock", fmt="q4_k_m", created_at="2026-03-01T00:00:00Z")
        self.add_entry("newer", source="uncensored", fmt="bf16", created_at="2026-02-01T00:00:00Z")
        self.add_entry("", created_at="2026-04-01T00:00:00Z")
        self.add_entry("short", msl=256, created_at="2026-05-01T00:00:00Z")
        (prompt_cache.library_of(LIB_9B).directory / "junk.msprompt").write_bytes(b"junk")
        listing = self.service.prompt_cache_list(self.lib_params(text_encoder_source="stock"))
        self.assertEqual(listing["library"], LIB_9B)
        self.assertEqual(listing["directory"], str(self.program_root / "prompt_cache" / LIB_9B))
        self.assertEqual([row["prompt"] for row in listing["texts"]], ["newer", "older"], "no empty text, no other length")
        newer = listing["texts"][0]
        self.assertEqual([entry["encoder_format"] for entry in newer["entries"]], ["q4_k_m", "bf16"], "ranked: the preferred source first")
        self.assertEqual(newer["selected"], newer["entries"][0])
        self.assertEqual(listing["texts"][1]["name"], "Old")
        self.assertEqual([item["file"] for item in listing["skipped"]], ["junk.msprompt"])
        self.assertTrue(listing["text_encoder_available"])
        self.assertIsNone(listing["legacy"])
        self.assertEqual(self.service.prompt_cache_list(self.lib_params(max_sequence_length=256))["texts"][0]["prompt"], "short")

    def test_the_listing_needs_no_transformer_and_names_a_library(self) -> None:
        self.add_entry()
        self.assertEqual(len(self.service.prompt_cache_list({"prompt_library": LIB_9B})["texts"]), 1)
        self.assertFalse(self.service.prompt_cache_list({"prompt_library": LIB_9B})["text_encoder_available"])
        with self.assertRaisesRegex(ValueError, "prompt_library"):
            self.service.prompt_cache_list({})
        self.assertEqual(self.service.prompt_cache_list({"prompt_library": LIB_4B})["texts"], [], "libraries are per model")


class ExportImportTests(_LibraryCase):
    """`.prompt_cache.export` (by file) and `.prompt_cache.import` (v2 into its own library, v1 by the migration rules)."""

    def test_export_copies_the_entry_byte_for_byte(self) -> None:
        entry = self.add_entry()
        dest = self.root / "out" / "copy.msprompt"
        dest.parent.mkdir()
        out = self.service.prompt_cache_export(self.lib_params(), entry.file, str(dest))
        self.assertEqual(out, {"library": LIB_9B, "file": entry.file, "path": str(dest), "size_bytes": entry.size_bytes})
        self.assertEqual(dest.read_bytes(), prompt_cache.library_of(LIB_9B).path_of(entry.file).read_bytes())

    def test_export_refusals(self) -> None:
        entry = self.add_entry()
        for file, path in (("missing.msprompt", str(self.root / "x.msprompt")), ("../x.msprompt", str(self.root / "x.msprompt")), (entry.file, "relative.msprompt"), (entry.file, str(self.root / "x.txt"))):
            with self.subTest(file=file, path=path), self.assertRaises(ValueError):
                self.service.prompt_cache_export(self.lib_params(), file, path)

    def _v2_file(self, library_id: str, prompt: str = "imported") -> Path:
        metadata = pl.build_metadata(prompt_cache.flux2_library_spec(library_id) if library_id in prompt_cache.FLUX2_LIBRARY_IDS else pl.LibrarySpec(library_id, "prompt_embeds", "bfloat16", "BF16"), prompt=prompt, name="Imp", max_sequence_length=512, encoder=pl.EncoderProvenance("stock", "bf16", False, "e" * 64, "te"), origin=pl.ORIGIN_BUILT)
        path = self.root / "outside" / f"{library_id}.msprompt"
        _write_prompt_cache_file(path, metadata, shape=[1, 512, 8])
        return path

    def test_a_v2_file_lands_in_its_own_library(self) -> None:
        path = self._v2_file(LIB_4B)
        out = self.service.prompt_cache_import(self.lib_params(), str(path))
        self.assertEqual((out["library"], out["library_matches"], out["already_present"], out["origin"], out["name"], out["prompt"]), (LIB_4B, False, False, "imported", "Imp", "imported"))
        self.assertIn(out["file"], prompt_cache.library_of(LIB_4B).reconcile().entries)
        self.assertTrue(self.service.prompt_cache_import(self.lib_params(prompt_library=LIB_4B), str(path))["already_present"])
        self.assertTrue(self.service.prompt_cache_import(self.lib_params(prompt_library=LIB_4B), str(path))["library_matches"])

    def test_foreign_files_are_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "а не к FLUX.2"):
            self.service.prompt_cache_import(self.lib_params(), str(self._v2_file("other_model")))
        junk = self.root / "junk.msprompt"
        junk.write_bytes(b"junk")
        with self.assertRaisesRegex(ValueError, "не является кэшем"):
            self.service.prompt_cache_import(self.lib_params(), str(junk))
        unmarked = self.root / "unmarked.msprompt"
        _write_prompt_cache_file(unmarked, {"format": "something"})
        with self.assertRaisesRegex(ValueError, "метки"):
            self.service.prompt_cache_import(self.lib_params(), str(unmarked))
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.lib_params(), "relative.msprompt")

    def test_a_v1_file_of_an_unknown_stock_encoder_is_migrated_on_import(self) -> None:
        path = self.root / "outside" / "my preset.msprompt"
        _write_prompt_cache_file(path, _v1_metadata(), shape=[1, 512, 12288])
        out = self.service.prompt_cache_import(self.lib_params(), str(path))
        self.assertEqual((out["library"], out["library_matches"], out["origin"], out["name"]), (LIB_9B, True, "migrated_v1_unverified", "my preset"))
        self.assertTrue(path.exists(), "an import copies; the source stays")

    def test_a_v1_file_made_before_the_rope_fix_is_refused_by_name(self) -> None:
        folder = self.root / "text_encoder"
        (folder / "config.json").write_text(json.dumps(load_9b_config("uncensored_text_encoder"), indent=2), encoding="utf-8")
        unsalted = svc.text_encoder_fingerprint(str(folder), rope_salt=False)
        path = self.root / "outside" / "old.msprompt"
        _write_prompt_cache_file(path, _v1_metadata(text_encoder_id=unsalted), shape=[1, 512, 12288])
        with self.assertRaisesRegex(ValueError, "RoPE"):
            self.service.prompt_cache_import(self.lib_params(), str(path))
        self.assertEqual(dict(prompt_cache.library_of(LIB_9B).reconcile().entries), {})


class ClassifyV1Tests(unittest.TestCase):
    """The v1 provenance table, pure."""

    KNOWN = {
        "salted": prompt_cache.KnownEncoder(True, True, "uncensored", "bf16", "FLUX.2-klein-9B/text_encoder_uncensored"),
        "unsalted": prompt_cache.KnownEncoder(False, True, "uncensored", "bf16", "FLUX.2-klein-9B/text_encoder_uncensored"),
        "stock": prompt_cache.KnownEncoder(True, False, "stock", "gguf", "q8.gguf"),
    }
    TENSOR = {"dtype": "BF16", "shape": [1, 512, 12288]}

    def verdict(self, tensor: dict[str, Any] | None = None, family: str = "qwen-te-ffffffff", **metadata: str) -> prompt_cache.V1Verdict:
        return prompt_cache.classify_v1_entry(_v1_metadata(**metadata), tensor or self.TENSOR, self.KNOWN, family=family)

    def test_the_table(self) -> None:
        self.assertEqual(self.verdict(text_encoder_id="unsalted").action, "delete")
        self.assertIn("RoPE", self.verdict(text_encoder_id="unsalted").reason)
        migrated = self.verdict(text_encoder_id="salted")
        self.assertEqual((migrated.action, migrated.library_id, migrated.origin, migrated.encoder.source, migrated.encoder.format), ("migrate", LIB_9B, "migrated_v1", "uncensored", "bf16"))
        stock = self.verdict(text_encoder_id="stock", text_encoder_format="q8_0")
        self.assertEqual((stock.encoder.source, stock.encoder.format), ("stock", "q8_0"), "the recorded v1 format wins")
        self.assertEqual(self.verdict(text_encoder_id="stock").encoder.format, "gguf")
        self.assertEqual(self.verdict(text_encoder_path="/home/u/flux2-klein-9b-uncensored-text-encoder/").action, "delete")
        self.assertEqual(self.verdict(family="flux2-klein-9b-uncensored-text-encoder-e52fb293").action, "delete")
        unverified = self.verdict()
        self.assertEqual((unverified.action, unverified.origin, unverified.encoder.source, unverified.encoder.label), ("migrate", "migrated_v1_unverified", "", "qwen-te"))
        self.assertEqual(self.verdict(tensor={"dtype": "BF16", "shape": [1, 512, 7680]}).library_id, LIB_4B)

    def test_what_is_kept(self) -> None:
        for label, verdict in (
            ("float16", self.verdict(dtype="float16")),
            ("F16 token", self.verdict(tensor={"dtype": "F16", "shape": [1, 512, 12288]})),
            ("width", self.verdict(tensor={"dtype": "BF16", "shape": [1, 512, 100]})),
            ("length", self.verdict(max_sequence_length="x")),
            ("no id", self.verdict(text_encoder_id="")),
        ):
            with self.subTest(label):
                self.assertEqual(verdict.action, "keep")

    def test_the_user_replica_is_deleted(self) -> None:
        # Its encoder folder is not on this machine and its family names the uncensored encoder.
        replica = {**_v1_metadata(), **_V1_UNCENSORED_REPLICA}
        verdict = prompt_cache.classify_v1_entry(replica, self.TENSOR, {}, family=replica["text_encoder_family"])
        self.assertEqual(verdict.action, "delete")


class MigrationTests(_LibraryCase):
    """`migrate_legacy_families` / `ensure_legacy_migrated` over a temp program root."""

    def setUp(self) -> None:
        super().setUp()
        encoder = self.installed_package_9b / "text_encoder_uncensored"
        encoder.mkdir(parents=True)
        (encoder / "config.json").write_text(json.dumps(load_9b_config("uncensored_text_encoder"), indent=2), encoding="utf-8")
        (encoder / "model.safetensors").write_bytes(b"\x00" * 64)
        self.salted = svc.text_encoder_fingerprint(str(encoder))
        self.unsalted = svc.text_encoder_fingerprint(str(encoder), rope_salt=False)
        self.assertNotEqual(self.salted, self.unsalted)
        self.root_dir = self.program_root / "prompt_cache"
        self.files = {
            "fam-aaaaaaaa/prefix.msprompt": (_v1_metadata(text_encoder_id=self.unsalted, prompt="pre"), [1, 512, 12288]),
            "fam-aaaaaaaa/post.msprompt": (_v1_metadata(text_encoder_id=self.salted, prompt="post prompt"), [1, 512, 12288]),
            "unc-bbbbbbbb/unknown.msprompt": (_v1_metadata(text_encoder_path="/home/u/flux2-klein-9b-uncensored-text-encoder/"), [1, 512, 12288]),
            "plain-cccccccc/plain.msprompt": (_v1_metadata(prompt="plain prompt"), [1, 512, 7680]),
            "plain-cccccccc/half.msprompt": (_v1_metadata(dtype="float16"), [1, 512, 12288]),
            "plain-cccccccc/odd.msprompt": (_v1_metadata(), [1, 512, 100]),
        }
        for where, (metadata, shape) in self.files.items():
            _write_prompt_cache_file(self.root_dir / where, metadata, shape=shape, fill=7)
        (self.root_dir / "plain-cccccccc" / "junk.msprompt").write_bytes(b"junk")

    def test_the_whole_table(self) -> None:
        data = (self.root_dir / "fam-aaaaaaaa" / "post.msprompt").read_bytes()[-64:]
        listing = self.service.prompt_cache_list(self.lib_params())
        self.assertEqual(listing["legacy"], {"migrated": 2, "deleted": 2, "not_migrated": 3, "directory": str(self.root_dir)})
        self.assertFalse((self.root_dir / "fam-aaaaaaaa").exists(), "emptied family directories are removed")
        self.assertFalse((self.root_dir / "unc-bbbbbbbb").exists())
        self.assertEqual(sorted(path.name for path in (self.root_dir / "plain-cccccccc").iterdir()), ["half.msprompt", "junk.msprompt", "odd.msprompt"])
        nine = prompt_cache.library_of(LIB_9B).snapshot()
        (post,) = nine.entries_of_text("post prompt")
        self.assertEqual((post.name, post.origin, post.encoder.source, post.encoder.encoder_id, post.created_at), ("post", "migrated_v1", "uncensored", self.salted, "2026-09-02T17:28:47Z"))
        self.assertEqual(prompt_cache.library_of(LIB_9B).path_of(post.file).read_bytes()[-64:], data, "tensor bytes verbatim")
        self.assertEqual(read_safetensors_header(prompt_cache.library_of(LIB_9B).path_of(post.file))["__metadata__"]["migrated_from"], "fam-aaaaaaaa/post.msprompt")
        self.assertEqual(listing["texts"][0]["prompt"], "post prompt")
        (plain,) = prompt_cache.library_of(LIB_4B).reconcile().entries_of_text("plain prompt")
        self.assertEqual((plain.origin, plain.encoder.source), ("migrated_v1_unverified", ""))

    def test_it_is_idempotent_and_runs_once_per_process(self) -> None:
        self.service.prompt_cache_list(self.lib_params())
        again = prompt_cache.migrate_legacy_families(self.root_dir, [])
        self.assertEqual((again.migrated, again.deleted, again.not_migrated), (0, 0, 3))
        with patch.object(prompt_cache, "migrate_legacy_families", side_effect=AssertionError("ran twice")):
            self.service.prompt_cache_list(self.lib_params())
        self.assertEqual(len(prompt_cache.library_of(LIB_9B).reconcile().entries), 1)

    def test_a_failed_verification_keeps_the_v1_file_and_deletions_still_happen(self) -> None:
        with patch.object(prompt_cache, "_verify_migrated", side_effect=ValueError("mismatch")):
            report = prompt_cache.migrate_legacy_families(self.root_dir, [])
        self.assertEqual((report.migrated, report.deleted, report.not_migrated), (0, 2, 5))
        self.assertTrue((self.root_dir / "fam-aaaaaaaa" / "post.msprompt").exists())
        self.assertFalse((self.root_dir / "fam-aaaaaaaa" / "prefix.msprompt").exists())

    # -- containment: no link is ever followed, read for migration or deleted --
    def _outside_v1(self) -> Path:
        """A directory OUTSIDE the cache holding a migratable and a pre-fix v1 file (both would be deleted if followed)."""
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        folder = Path(tmp.name)
        _write_prompt_cache_file(folder / "post.msprompt", _v1_metadata(text_encoder_id=self.salted, prompt="foreign post"), shape=[1, 512, 12288])
        _write_prompt_cache_file(folder / "prefix.msprompt", _v1_metadata(text_encoder_id=self.unsalted, prompt="foreign pre"), shape=[1, 512, 12288])
        return folder

    def _symlink_or_skip(self, link: Path, target: Path, *, directory: bool) -> None:
        try:
            os.symlink(target, link, target_is_directory=directory)
        except (OSError, NotImplementedError) as exc:
            self.skipTest(f"symbolic links unavailable: {exc}")

    def test_a_linked_family_is_reported_and_never_followed(self) -> None:
        outside = self._outside_v1()
        self._symlink_or_skip(self.root_dir / "evil-dddddddd", outside, directory=True)
        report = prompt_cache.migrate_legacy_families(self.root_dir, [])
        self.assertEqual(sorted(path.name for path in outside.iterdir()), ["post.msprompt", "prefix.msprompt"])
        self.assertTrue((self.root_dir / "evil-dddddddd").is_symlink())
        self.assertIn("evil-dddddddd", [where for where, _reason in report.failures])
        self.assertEqual((report.migrated, report.deleted, report.not_migrated), (2, 2, 4))
        self.assertEqual(prompt_cache.library_of(LIB_9B).reconcile().entries_of_text("foreign post"), ())

    def test_a_linked_file_in_a_real_family_is_reported_and_kept(self) -> None:
        outside = self._outside_v1()
        link = self.root_dir / "fam-aaaaaaaa" / "link.msprompt"
        self._symlink_or_skip(link, outside / "prefix.msprompt", directory=False)
        report = prompt_cache.migrate_legacy_families(self.root_dir, [])
        self.assertTrue(link.is_symlink())
        self.assertEqual(sorted(path.name for path in outside.iterdir()), ["post.msprompt", "prefix.msprompt"])
        self.assertIn("fam-aaaaaaaa/link.msprompt", [where for where, _reason in report.failures])
        self.assertEqual((report.migrated, report.deleted, report.not_migrated), (2, 2, 4))
        self.assertEqual(sorted(path.name for path in (self.root_dir / "fam-aaaaaaaa").iterdir()), ["link.msprompt"], "a family holding a link is not removed")

    def test_a_linked_root_migrates_and_deletes_nothing(self) -> None:
        linked = self.program_root / "linked_cache"
        self._symlink_or_skip(linked, self.root_dir, directory=True)
        before = sorted(str(path.relative_to(self.root_dir)) for path in self.root_dir.rglob("*"))
        report = prompt_cache.migrate_legacy_families(linked, [])
        self.assertEqual((report.migrated, report.deleted, report.not_migrated), (0, 0, 1))
        self.assertEqual(sorted(str(path.relative_to(self.root_dir)) for path in self.root_dir.rglob("*")), before)

    def test_a_dot_dot_path_is_never_deleted(self) -> None:
        victim = self.root_dir / "plain-cccccccc" / "half.msprompt"
        with self.assertRaises(pl.UnsafeLibraryPath):
            pl.remove_confined_file(self.root_dir / "fam-aaaaaaaa" / ".." / "plain-cccccccc" / "half.msprompt", self.root_dir)
        with self.assertRaises(pl.UnsafeLibraryPath):
            pl.remove_confined_file(self.root_dir / ".." / "prompt_cache" / "plain-cccccccc" / "half.msprompt", self.root_dir)
        self.assertTrue(victim.exists())

    # -- durability: the v2 entry is on stable storage before the v1 file goes --
    def _record(self) -> list[tuple[str, str]]:
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

    def test_the_v2_entry_is_durable_before_its_v1_file_is_unlinked(self) -> None:
        events = self._record()
        prompt_cache.migrate_legacy_families(self.root_dir, [])
        library = prompt_cache.library_of(LIB_9B)
        (post,) = library.reconcile().entries_of_text("post prompt")
        family = self.root_dir / "fam-aaaaaaaa"
        replaced = events.index(("replace", str(library.path_of(post.file))))
        unlinked = events.index(("unlink", str(family / "post.msprompt")))
        self.assertLess(replaced, unlinked)
        self.assertEqual(events[replaced + 1], ("sync", str(library.directory)), "the publish itself syncs the library directory")
        self.assertIn(("sync", str(self.root_dir)), events[replaced:unlinked], "the root holding the library directory too")
        self.assertEqual(events[unlinked + 1], ("sync", str(family)), "the removal is synced in the family")
        deleted = events.index(("unlink", str(family / "prefix.msprompt")))
        self.assertEqual(events[deleted + 1], ("sync", str(family)))
        removed = events.index(("rmdir", str(family)))
        self.assertEqual(events[removed + 1], ("sync", str(self.root_dir)))

    def test_a_failed_directory_sync_keeps_the_v1_file(self) -> None:
        library_dir = prompt_cache.library_of(LIB_9B).directory
        real_sync = pl.sync_directory

        def flaky(directory: Path) -> None:
            if Path(directory) == library_dir:
                raise OSError("fsync failed")
            real_sync(directory)

        with patch.object(pl, "sync_directory", flaky):
            report = prompt_cache.migrate_legacy_families(self.root_dir, [])
        self.assertTrue((self.root_dir / "fam-aaaaaaaa" / "post.msprompt").exists(), "unproven durability never deletes the only copy")
        self.assertEqual((report.migrated, report.deleted), (1, 2), "the 4B entry lives in another directory and migrates")

    def test_a_build_triggers_it_and_a_request_without_a_library_never_does(self) -> None:
        self.service.status(self.params(prompt="x"))
        self.assertIsNone(prompt_cache.migration_report())
        self.seed_current()
        self.service.prompt_cache_build(self.lib_params(prompt="a cat"))
        self.assertEqual(prompt_cache.migration_report().migrated, 2)


# ---------------------------------------------------------------------------
# Managed runs (real torch, real safetensors files)
# ---------------------------------------------------------------------------
def _region_and_mask() -> tuple[bytes, bytes]:
    region = np.full((128, 128, 3), 90, dtype=np.uint8)
    mask = np.zeros((128, 128), dtype=np.uint8)
    mask[48:80, 48:80] = 255
    return _png_bytes(region, "RGB"), _png_bytes(mask, "L")


class PromptCacheBuildTests(_ManagedRunFixture):
    """`prompt_cache.build`: encode the prompt through the encoder handle only, save it, then let the encoder go."""

    def _params(self, **overrides: object) -> dict[str, object]:
        return self.params(**{"prompt_library": LIB_9B, **overrides})

    def _encoder_held(self) -> bool:
        return any(sum(held) > 0 for key, held in self.held().items() if key.startswith("flux2_klein_text_encoder:"))

    def _pipeline_held(self) -> bool:
        return any(sum(held) > 0 for key, held in self.held().items() if key.startswith("flux2_klein:"))

    def _run(self, prompt: str) -> None:
        region, mask = _region_and_mask()
        self.service.inpaint_image_bytes(region, mask, params=self._params(prompt=prompt))

    def test_it_encodes_and_saves_the_prompt_without_admitting_a_pipeline(self) -> None:
        out = self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertTrue(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertEqual(len(out["written"]), 1)
        self.assertEqual([call["prompt"] for call in self.pipeline_cls.encode_calls], ["a cat"])
        self.assertFalse(self._pipeline_held())
        self.assertFalse(any(entry["model"].startswith("flux2_klein") and not entry["model"].startswith("flux2_klein_text") for entry in self.large.status()))
        entry = prompt_cache.library_of(LIB_9B).reconcile().entries[out["written"][0]["file"]]
        self.assertEqual((entry.shape, entry.encoder.format, entry.encoder.source), ((1, 512, 24), "bf16", ""))

    def test_it_releases_the_encoder_it_loaded(self) -> None:
        self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertFalse(self._encoder_held())

    def test_an_encoder_that_was_already_resident_is_left_alone(self) -> None:
        self._run("first")  # a normal run keeps the encoder by default
        self.assertTrue(self._encoder_held())
        self.service.prompt_cache_build(self._params(prompt="second"))
        self.assertTrue(self._encoder_held())

    def test_a_run_s_embedding_is_saved_without_a_second_encode(self) -> None:
        self._run("a cat")
        self.pipeline_cls.encode_calls.clear()
        out = self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertEqual((out["encoded"], len(out["written"])), (False, 1))
        self.assertEqual(self.pipeline_cls.encode_calls, [])

    def test_a_second_build_of_the_same_prompt_reads_nothing(self) -> None:
        self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.pipeline_cls.encode_calls.clear()
        out = self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertEqual((out["encoded"], out["written"]), (False, []))
        self.assertEqual(self.pipeline_cls.encode_calls, [])
        self.assertFalse(self._encoder_held())

    def test_an_empty_prompt_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_build(self._params(prompt="   "))

    def test_the_status_flag_follows_the_library(self) -> None:
        with patch.object(hardware, "memory_snapshot", lambda *_a, **_k: {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}):
            self.service.prompt_cache_build(self._params(prompt="a cat"))
            self.assertTrue(self.service.status(self._params(prompt="a cat"))["prompt_cached"])
            self.assertFalse(self.service.status(self._params(prompt="a dog"))["prompt_cached"])
            self.assertFalse(self.service.status(self._params(prompt="a cat", max_sequence_length=256))["prompt_cached"])
            self.assertFalse(self.service.status(self._params(prompt="  "))["prompt_cached"])
            self.assertFalse(self.service.status(None)["prompt_cached"])

    def test_it_streams_the_prompt_phase_steps(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        self.service.prompt_cache_build(self._params(prompt="a cat"), progress_callback=lambda *frame: frames.append(frame))
        self.assertTrue(all(phase == "load" for phase, _s, _t, _l in frames))
        steps = [step for _p, step, _t, _l in frames]
        self.assertEqual(steps, [svc.LOAD_STEP_PREPARE, svc.LOAD_STEP_TEXT_ENCODER, svc.LOAD_STEP_ENCODE, svc.LOAD_STEP_ENCODER_DONE])

    def test_the_forecast_charges_the_build_with_the_encoder_alone(self) -> None:
        forecast = svc.forecast_memory(svc.normalize_flux2_klein_params(self.params()), 384, 384)
        standalone = forecast["phases"]["encode_standalone"]
        encode = forecast["phases"]["encode"]
        self.assertLess(standalone["vram_bytes"], encode["vram_bytes"], "no pipeline beside the encoder")
        self.assertLessEqual(standalone["ram_bytes"], encode["ram_bytes"])

    def test_a_machine_short_of_memory_refuses_the_build_before_reading_anything(self) -> None:
        self.probes.device_total = 1 << 10
        self.probes.host_available = 1 << 10
        with self.assertRaises(RuntimeError):
            self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertEqual(self.pipeline_cls.encode_calls, [])
        self.assertFalse(self._encoder_held())


class LibraryRunTests(_ManagedRunFixture):
    """A run served from the library: no encoder needed, a vanished file re-plans."""

    def _params(self, **overrides: object) -> dict[str, object]:
        return self.params(**{"prompt_library": LIB_9B, "prompt": "a cat", **overrides})

    def _without_encoder(self, **overrides: object) -> dict[str, object]:
        params = self._params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _fresh_service(self) -> None:
        """A new service over the same manager: its memory is empty, so a run must read the library."""
        self.service = svc.Flux2KleinInpaintService(self.small, large_models=self.large)

    def _run(self, params: dict[str, object]) -> dict[str, Any]:
        region, mask = _region_and_mask()
        return self.service.inpaint_image_bytes(region, mask, params=params)

    def _profiles(self, params: dict[str, object]) -> dict[str, Any]:
        from modules.ai_backend.inpaint.flux2_klein import large_model
        from modules.ai_backend.runtime.large_models.tiers import AUTO_STEPS

        shape = large_model.request_shape(128, 128, with_reference=False, max_sequence_length=512, steps=1)
        return self.service.large_model_profiles(params, shape, format=None, steps=AUTO_STEPS)

    def test_an_all_cached_run_needs_no_encoder_at_all(self) -> None:
        self.service.prompt_cache_build(self._params())
        self._fresh_service()
        self.pipeline_cls.encode_calls.clear()
        params = self._without_encoder()
        self.assertIsNone(self._profiles(params)["encoder"])
        out = self._run(params)
        self.assertEqual(out["region_size"], [128, 128])
        self.assertEqual(self.pipeline_cls.encode_calls, [])
        # The build's own encoder handle was released (it may stay listed with no bytes).
        self.assertFalse(any(sum(held) > 0 for key, held in self.held().items() if key.startswith("flux2_klein_text_encoder:")))

    def test_a_cached_text_with_a_resident_encoder_still_counts_it(self) -> None:
        self.service.prompt_cache_build(self._params())
        self.service.component_action(self._params(), component="text_encoder", action="load")
        self.assertIsNotNone(self._profiles(self._params())["encoder"])

    def test_a_library_read_is_remembered(self) -> None:
        self.service.prompt_cache_build(self._params())
        self._fresh_service()
        calls: list[str] = []
        real = pl.load_tensor

        def counting(path: Path, name: str) -> Any:
            calls.append(path.name)
            return real(path, name)

        with patch.object(pl, "load_tensor", counting):
            self._run(self._params())
            self._run(self._params())
        self.assertEqual(len(calls), 1)

    def _vanishing(self) -> Any:
        def vanish(path: Path, _name: str) -> Any:
            path.unlink()
            raise FileNotFoundError(path)

        return patch.object(pl, "load_tensor", vanish)

    def test_a_file_that_vanished_after_the_plan_is_re_encoded(self) -> None:
        self.service.prompt_cache_build(self._params())
        self._fresh_service()
        self.pipeline_cls.encode_calls.clear()
        with self._vanishing():
            out = self._run(self._params())
        self.assertEqual(out["region_size"], [128, 128])
        self.assertEqual([call["prompt"] for call in self.pipeline_cls.encode_calls], ["a cat"])

    def test_without_an_encoder_a_vanished_file_is_named(self) -> None:
        file = self.service.prompt_cache_build(self._params())["written"][0]["file"]
        self._fresh_service()
        with self._vanishing(), self.assertRaisesRegex(ValueError, file.replace(".", r"\.")):
            self._run(self._without_encoder())


class NoTextEncoderTests(_ManagedRunFixture):
    """A machine where the 16 GB Qwen3 was never downloaded.

    The contract: a cached prompt generates (the denoise and the VAE decode never
    look at the encoder), and everything that would have to ENCODE is refused
    with a message naming both ways out, before any weight is read.
    """

    def _params(self, **overrides: object) -> dict[str, object]:
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _cache(self, params: dict[str, object]) -> dict[str, Any]:
        import torch

        normalized = svc.normalize_flux2_klein_params(params)
        return seed_memory(self.service, params, tensor=torch.zeros((1, int(normalized["max_sequence_length"]), 24), dtype=torch.bfloat16))

    def _run(self, params: dict[str, object]) -> dict[str, Any]:
        region, mask = _region_and_mask()
        return self.service.inpaint_image_bytes(region, mask, params=params)

    def test_an_absent_encoder_path_normalizes_instead_of_raising(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self._params(prompt="a cat"))
        self.assertEqual(normalized["text_encoder_path"], "")
        for key in ("transformer_path", "vae_path"):
            with self.subTest(key=key):
                params = self._params()
                params.pop(key)
                with self.assertRaises(ValueError):
                    svc.normalize_flux2_klein_params(params)

    def test_a_path_that_is_not_on_disk_counts_as_no_encoder(self) -> None:
        params = self.params(text_encoder_path=str(self.root / "gone"))
        self.assertFalse(svc.text_encoder_available(svc.normalize_flux2_klein_params(params)))
        self.assertIsNone(svc.encoder_provenance(svc.normalize_flux2_klein_params(params)))
        self.assertTrue(svc.text_encoder_available(svc.normalize_flux2_klein_params(self.params())))

    def test_an_encoder_that_exists_but_cannot_be_identified_is_still_an_error(self) -> None:
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.encoder_provenance(svc.normalize_flux2_klein_params(self.params()))

    def test_a_cached_prompt_is_served_without_touching_the_encoder(self) -> None:
        params = self._params(prompt="a cat")
        self._cache(params)
        out = self._run(params)
        self.assertEqual(out["region_size"], [128, 128])
        self.assertEqual(self.pipeline_cls.encode_calls, [])
        self.assertFalse(any(key.startswith("flux2_klein_text_encoder:") for key in self.held()))

    def test_an_uncached_prompt_is_refused_naming_both_ways_out_before_any_load(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self._run(self._params(prompt="a cat"))
        message = str(caught.exception)
        self.assertIn("энкодер", message)
        self.assertIn("кэш", message)
        self.assertEqual(self.large.status(), [], "nothing was admitted")

    def test_the_refusal_names_a_configured_path_that_is_missing(self) -> None:
        missing = str(self.root / "gone")
        with self.assertRaises(ValueError) as caught:
            self._run(self.params(prompt="a cat", text_encoder_path=missing))
        self.assertIn(missing, str(caught.exception))

    def test_build_without_an_encoder_is_refused_even_for_a_cached_prompt(self) -> None:
        # A library entry must name the encoder that made it; without one there is nothing to record.
        self._cache(self._params(prompt="a cat", prompt_library=LIB_9B))
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_build(self._params(prompt="a cat", prompt_library=LIB_9B))
        self.assertIn("энкодер", str(caught.exception))
        self.assertEqual(self.large.status(), [])


if __name__ == "__main__":
    unittest.main()
