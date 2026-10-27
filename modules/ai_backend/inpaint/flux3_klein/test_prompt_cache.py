"""
File: modules/ai_backend/inpaint/flux3_klein/test_prompt_cache.py

Purpose:
Tests of FLUX.3 klein's prompt cache (`prompt_cache.py` and the service's
cache paths, B4): the encoder identity (D11 fingerprint inputs: shards vs
GGUF, a config / chat-template / size change, absent weights), the width read,
the adapter; over the real service and manager (`ServiceRig`): a memory hit
after a plain run, a library hit, THE user's rule — a cached TEXT run with the
WHOLE `text_encoder/` directory deleted acquires no encoder session, calls no
encoder / processor / tokenizer loader, holds and forecasts nothing of the
encoder (`large_model_profiles` `encoder` is `None`) while `.status` keeps the
DiT rows available — a
resident encoder over a cached caption counted but not touched, `vl_refs`
never served or remembered, the byte cap of the memory, a library file gone
between plan and read (re-plan, then encode or refuse), the observer contract
of a served run (`cached` action, no encoder forecast), `.status` keys, and the
library surface (build / rename / already, the build's own encoder release,
its refusals, list, export, import of a foreign library and of a v1 file).

Notes:
- CPU only, tiny stand-ins; every file (package, library root) lives in a
  temporary directory the test removes. The library root is redirected by
  patching `prompt_cache.program_root`.
- The rig's package has no `text_encoder/config.json` (the rig's encoder is
  then unidentifiable and never remembered); `_CacheCase` writes one and a
  chat template so the encoder has an identity.
"""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import torch
from safetensors.torch import save_file

from ...runtime import prompt_library as pl
from ...runtime.large_models.manager import LargeModelRefused
from ...runtime.large_models.spec import RequestShape
from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from . import prompt_cache, service, text_encoder
from ._service_test_fixtures import TEXT_LEN, Recorder, ServiceRig, _save
from .config import ENCODER_BF16_SOURCE, EncoderSource, WeightFormat
from .params import EncoderMode, Flux3Options, Flux3Request
from .phases import Phase
from .prompt_cache import LIBRARY_ID, Flux3CacheQuery, Flux3PromptCacheAdapter, Flux3PromptCacheRequest

LIBRARY = Flux3PromptCacheRequest(LIBRARY_ID, ("bf16", "q8_0", "q4_k_m"))


def _write_identity(root: Path, *, config: str = '{"model_type": "qwen3_vl"}', template: str | None = '{"chat_template": "x"}') -> None:
    """Give the package's `text_encoder/` the files of the encoder identity."""
    directory = root / "text_encoder"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "config.json").write_text(config, encoding="utf-8")
    if template is not None:
        (directory / "chat_template.json").write_text(template, encoding="utf-8")


class IdentityTests(unittest.TestCase):
    """The D11 fingerprint inputs, the provenance, the width read and the adapter (no service)."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="flux3_identity_")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        _write_identity(self.root)
        shard = "model-00001-of-00001.safetensors"
        _save(self.root / "text_encoder" / shard, {"model.language_model.layers.0.weight": torch.ones(4, 4)})
        (self.root / "text_encoder" / "model.safetensors.index.json").write_text(json.dumps({"weight_map": {"model.language_model.layers.0.weight": shard}}), encoding="utf-8")
        self.gguf = EncoderSource("q8_0", self.root / "text_encoder_gguf" / "enc-Q8_0.gguf")
        self.gguf.path.parent.mkdir()
        self.gguf.path.write_bytes(b"GGUF" + b"\0" * 28)

    def test_shards_and_a_gguf_have_different_identities(self) -> None:
        bf16 = prompt_cache.text_encoder_fingerprint(self.root, ENCODER_BF16_SOURCE)
        gguf = prompt_cache.text_encoder_fingerprint(self.root, self.gguf)
        self.assertNotEqual(bf16, gguf)
        self.assertEqual(bf16, prompt_cache.text_encoder_fingerprint(self.root, ENCODER_BF16_SOURCE), "stable")

    def test_every_input_changes_the_identity(self) -> None:
        before = {source.id: prompt_cache.text_encoder_fingerprint(self.root, source) for source in (ENCODER_BF16_SOURCE, self.gguf)}
        for change in ("config", "template", "template_absent", "size"):
            with self.subTest(change=change):
                _write_identity(self.root)
                if change == "config":
                    _write_identity(self.root, config='{"model_type": "qwen3_vl", "x": 1}')
                elif change == "template":
                    _write_identity(self.root, template='{"chat_template": "y"}')
                elif change == "template_absent":
                    (self.root / "text_encoder" / "chat_template.json").unlink()
                else:
                    self.gguf.path.write_bytes(b"GGUF" + b"\0" * 60)
                    _save(self.root / "text_encoder" / "model-00001-of-00001.safetensors", {"model.language_model.layers.0.weight": torch.ones(8, 4)})
                after = {source.id: prompt_cache.text_encoder_fingerprint(self.root, source) for source in (ENCODER_BF16_SOURCE, self.gguf)}
                self.assertNotEqual(after["bf16"], before["bf16"])
                self.assertNotEqual(after["q8_0"], before["q8_0"])

    def test_absent_weights_have_no_provenance_and_present_ones_are_stock(self) -> None:
        provenance = prompt_cache.encoder_provenance(self.root, self.gguf)
        assert provenance is not None
        self.assertEqual((provenance.source, provenance.format, provenance.fp8, provenance.label), ("stock", "q8_0", False, "enc-Q8_0.gguf"))
        self.assertEqual(prompt_cache.encoder_provenance(self.root, ENCODER_BF16_SOURCE).label, f"{self.root.name}/text_encoder")  # type: ignore[union-attr]
        self.gguf.path.unlink()
        (self.root / "text_encoder" / "model-00001-of-00001.safetensors").unlink()
        for source in (self.gguf, ENCODER_BF16_SOURCE):
            with self.subTest(source=source.id):
                self.assertIsNone(prompt_cache.encoder_provenance(self.root, source))
                self.assertFalse(prompt_cache.encoder_available(self.root, source))
        with self.assertRaises(FileNotFoundError):
            prompt_cache.text_encoder_fingerprint(self.root, ENCODER_BF16_SOURCE)

    def test_an_unreadable_config_is_unidentifiable(self) -> None:
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            prompt_cache.encoder_provenance(self.root, ENCODER_BF16_SOURCE)

    def test_the_width_is_read_leniently(self) -> None:
        self.assertIsNone(prompt_cache.text_width(None))
        self.assertIsNone(prompt_cache.text_width(self.root), "no config.json")
        for content, expected in (('{"dit": {"in_channels": {"text": 20480}}}', 20480), ("{", None), ('{"dit": {"in_channels": {"text": true}}}', None), ('{"dit": []}', None)):
            with self.subTest(content=content):
                (self.root / "config.json").write_text(content, encoding="utf-8")
                self.assertEqual(prompt_cache.text_width(self.root), expected)

    def test_the_adapter(self) -> None:
        adapter = Flux3PromptCacheAdapter()
        query = Flux3CacheQuery("a cat", LIBRARY, self.root, ENCODER_BF16_SOURCE, 20480)
        self.assertEqual((adapter.library_id(query), adapter.texts_needed(query), adapter.max_sequence_length(query), adapter.accepted_width(query)), (LIBRARY_ID, ["a cat"], 4096, 20480))
        self.assertEqual(adapter.preference(query), pl.Preference("stock", ("bf16", "q8_0", "q4_k_m")))
        self.assertTrue(adapter.accepts_library(LIBRARY_ID))
        self.assertFalse(adapter.accepts_library("flux2_klein_9b"))
        self.assertIsNone(adapter.library_id(Flux3CacheQuery("a cat", Flux3PromptCacheRequest(), self.root, ENCODER_BF16_SOURCE, None)))
        self.assertIsNone(adapter.current_provenance(Flux3CacheQuery("a cat", LIBRARY, None, ENCODER_BF16_SOURCE, None)))
        self.assertTrue(adapter.encoder_available(query))
        self.assertIsNone(adapter.legacy_report())
        with self.assertRaises(ValueError):
            adapter.import_foreign({"__metadata__": {"format": "manhwastudio.flux2_klein.prompt_cache"}}, self.root / "x.msprompt", query)


class _CacheCase(unittest.TestCase):
    """A `ServiceRig` whose encoder is identifiable, a temporary library root and an admission spy."""

    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R,))
        _write_identity(self.rig.root)
        library_root = tempfile.TemporaryDirectory(prefix="flux3_prompt_library_")
        self.addCleanup(library_root.cleanup)
        self.program_root = Path(library_root.name)
        patcher = patch.object(prompt_cache, "program_root", lambda: self.program_root)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.service = self.rig.service
        self.recorder = Recorder()
        self.admitted: list[str] = []
        real_acquire = self.rig.manager.acquire

        def acquire(model: str, *args: Any, **kwargs: Any) -> Any:
            self.admitted.append(model)
            return real_acquire(model, *args, **kwargs)

        patcher = patch.object(self.rig.manager, "acquire", acquire)
        patcher.start()
        self.addCleanup(patcher.stop)

    def _options(self, **overrides: Any) -> Flux3Options:
        return Flux3Options(model_root=self.rig.root, variant=WeightFormat.FP8R, **overrides)

    def _run(self, caption: str = "a cat", *, options: Flux3Options | None = None, cache: Flux3PromptCacheRequest | None = LIBRARY, references: tuple[Any, ...] = ()) -> service.Flux3Result:
        self.recorder.events.clear()
        self.admitted.clear()
        return self.service.generate(Flux3Request(caption=caption, references=references), options or self._options(), observer=self.recorder, prompt_cache=cache)

    def _params(self, **overrides: Any) -> dict[str, Any]:
        return {"source_root": str(self.rig.root), "format": "fp8r", "prompt": "a cat", "prompt_library": LIBRARY_ID, "prompt_cache_precision_order": ["bf16", "q8_0", "q4_k_m"], **overrides}

    def _encoder_admitted(self) -> bool:
        return any(model.startswith("flux3_klein_text_encoder.") for model in self.admitted)

    def _encoder_key(self, options: Flux3Options | None = None) -> str:
        return service._encoder_handle_key(options or self._options(), "cpu")

    def _delete_encoder_weights(self) -> None:
        directory = self.rig.root / "text_encoder"
        for name in ("model-00001-of-00001.safetensors", "model.safetensors.index.json"):
            (directory / name).unlink()

    def _profiles(self, **params: Any) -> dict[str, Any]:
        shape = RequestShape(64, 64)
        return self.service.large_model_profiles(self._params(**params), shape, format=None, steps=Steps(VramStep.MAX, RamStep.KEEP))


class ServedRunTests(_CacheCase):
    def test_a_repeated_caption_is_served_from_memory_without_the_encoder(self) -> None:
        self._run(cache=None)
        self.assertTrue(self._encoder_admitted())
        self.assertEqual(len(self.rig.encode_calls), 1)
        result = self._run(cache=None)
        self.assertFalse(self._encoder_admitted(), "a served caption opens no encoder handle")
        self.assertEqual(len(self.rig.encode_calls), 1)
        # Nothing under `text_encoder/` is read for a served caption: the real count is the stored length.
        self.assertEqual((result.text_tokens, result.text_real_tokens), (TEXT_LEN, TEXT_LEN))
        self.assertEqual([(phase, event) for phase, event, _info in self.recorder.events], [(phase, event) for phase in Phase for event in ("begin", "end")])
        load = self.recorder.begin(Phase.ENCODER_LOAD)
        self.assertEqual(load["action"], "cached")
        for phase in (Phase.ENCODER_LOAD, Phase.ENCODE, Phase.ENCODER_RELEASE):
            info = self.recorder.begin(phase)
            self.assertEqual((info["forecast_device_peak"], info["forecast_host_peak"]), (None, None), phase)
        self.assertEqual((self.recorder.begin(Phase.ENCODER_RELEASE)["text_tokens"], self.recorder.begin(Phase.DENOISE)["text_real_tokens"]), (TEXT_LEN, TEXT_LEN))
        # Not written to the library: memory only (no library given).
        self.assertFalse((self.program_root / "prompt_cache").exists())

    def test_another_caption_or_library_is_encoded(self) -> None:
        self._run("a cat", cache=None)
        self._run("a dog", cache=None)
        self._run("a cat", cache=LIBRARY)
        self.assertEqual(len(self.rig.encode_calls), 3, "memory entries are per library id")

    def test_a_cached_text_run_needs_no_encoder_on_disk(self) -> None:
        # THE user's rule: build into the library, drop every handle, delete the WHOLE encoder directory.
        self.service.prompt_cache_build(self._params())
        self.service.unload()
        shutil.rmtree(self.rig.root / "text_encoder")
        fresh = service.Flux3KleinService(self.rig.manager)
        self.service = fresh

        def forbidden(*_args: Any, **_kwargs: Any) -> Any:
            raise AssertionError("a served caption must read nothing of the encoder")

        for name in ("load_text_processor", "build_text_encoder_meta", "build_gguf_text_encoder_meta", "caption_token_count", "tokenize_caption", "encode_text", "require_gguf_text_encoder"):
            patcher = patch.object(text_encoder, name, forbidden)
            patcher.start()
            self.addCleanup(patcher.stop)
        status = fresh.status(self._params())
        self.assertEqual((status["available"], status["formats"]["fp8r"]["available"]), (True, True), "the DiT rows never depend on the encoder")
        self.assertEqual((status["text_encoder_present"], status["text_encoder_formats"]["bf16"]["available"], status["prompt_cached"]), (False, False, True))
        self.assertEqual(status["prompt_cache_entry"]["origin"], "built")
        profiles = self._profiles()
        self.assertIsNone(profiles["encoder"], "nothing of the encoder is forecast")
        self.assertEqual(profiles["encoder_formats"], [])
        self.assertTrue(profiles["selected"]["fits"])
        result = self._run()
        self.assertFalse(self._encoder_admitted())
        self.assertEqual(self.rig.manager.placements(self._encoder_key()), {})
        self.assertIsNone(fresh.health()["encoder_resident"])
        self.assertEqual(result.text_tokens, TEXT_LEN)
        self.assertEqual(self.recorder.begin(Phase.ENCODER_LOAD)["action"], "cached")
        # Another caption must encode, and is refused naming the missing encoder (before any admission).
        with self.assertRaises(FileNotFoundError) as caught:
            self._run("a dog")
        self.assertIn("text_encoder_format", str(caught.exception))
        self.assertEqual(self.admitted, [])

    def test_a_resident_encoder_over_a_cached_caption_is_counted_but_not_touched(self) -> None:
        options = self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP, encoder_on_cpu=False)
        self._run(options=options)
        self.assertTrue(self.rig.manager.placements(self._encoder_key(options)))
        profiles = self._profiles(vram_step="max", ram_step="keep", encoder_on_cpu=False)
        self.assertIsNotNone(profiles["encoder"], "a resident encoder is counted")
        self._run(options=options)
        self.assertFalse(self._encoder_admitted())
        self.assertEqual(len(self.rig.encode_calls), 1)
        self.assertEqual(self.recorder.begin(Phase.ENCODER_LOAD)["action"], "cached")

    def test_a_not_resident_encoder_over_a_cached_caption_is_not_counted(self) -> None:
        self.assertIsNotNone(self._profiles()["encoder"], "uncached: counted")
        self._run(options=self._options(vram_step=VramStep.PHASE, ram_step=RamStep.MIN))
        self.assertIsNone(self.service.health()["encoder_resident"], "«Минимум» RAM drops the encoder after the run")
        self.assertIsNone(self._profiles()["encoder"])
        self.assertIsNotNone(self._profiles(prompt="another caption")["encoder"])

    def test_vl_refs_is_never_served_nor_remembered(self) -> None:
        from PIL import Image

        options = self._options(encoder_mode=EncoderMode.VL_REFS)
        reference = (Image.new("RGB", (64, 64), (1, 2, 3)),)
        self._run(options=options, references=reference)
        self._run(options=options, references=reference)
        self.assertEqual([kind for kind, _device in self.rig.encode_calls], ["vl", "vl"])
        self.assertTrue(self._encoder_admitted())
        self.assertFalse((self.program_root / "prompt_cache").exists())
        # A TEXT entry of the same caption does not serve VL_REFS either.
        self.service.prompt_cache_build(self._params())
        self._run(options=options, references=reference)
        self.assertEqual(len(self.rig.encode_calls), 4)
        status = self.service.status(self._params(encoder_mode="vl_refs"))
        self.assertEqual((status["prompt_cached"], status["prompt_cache_entry"]), (False, None))
        self.assertIsNotNone(self._profiles(encoder_mode="vl_refs")["encoder"])

    def test_the_memory_is_byte_capped(self) -> None:
        with patch.object(service, "PROMPT_EMBED_CACHE_BYTES", 1):
            capped = service.Flux3KleinService(self.rig.manager)
        self.service = capped
        for caption in ("a cat", "a dog", "a cat"):
            self._run(caption, cache=None)
        self.assertEqual(len(self.rig.encode_calls), 3, "the newest entry alone stays under a 1-byte cap")
        self.service = service.Flux3KleinService(self.rig.manager)
        for caption in ("a cat", "a dog", "a cat"):
            self._run(caption, cache=None)
        self.assertEqual(len(self.rig.encode_calls), 5, "the default cap keeps both")

    def test_an_unidentifiable_encoder_still_runs_and_is_not_remembered(self) -> None:
        (self.rig.root / "text_encoder" / "config.json").unlink()
        self._run(cache=None)
        self._run(cache=None)
        self.assertEqual(len(self.rig.encode_calls), 2)


class GoneFileTests(_CacheCase):
    def _gone(self) -> Any:
        """Make every library read find its file gone (and delete the files, so a re-plan finds nothing)."""

        def read_tensor(library: pl.PromptLibrary, entry: pl.LibraryEntry) -> Any:
            for path in library.directory.glob("*.msprompt"):
                path.unlink()
            library.reconcile()
            raise pl.LibraryEntryGone(f"{entry.file} gone")

        return patch.object(pl.PromptLibrary, "read_tensor", read_tensor)

    def test_a_gone_file_re_plans_and_encodes(self) -> None:
        self.service.prompt_cache_build(self._params())
        fresh = service.Flux3KleinService(self.rig.manager)
        self.service = fresh
        calls = len(self.rig.encode_calls)
        with self._gone():
            self._run()
        self.assertEqual(len(self.rig.encode_calls), calls + 1)
        self.assertTrue(self._encoder_admitted())

    def test_a_gone_file_without_an_encoder_is_refused(self) -> None:
        self.service.prompt_cache_build(self._params())
        self.service.unload()
        self._delete_encoder_weights()
        self.service = service.Flux3KleinService(self.rig.manager)
        with self._gone(), self.assertRaises(ValueError) as caught:
            self._run()
        self.assertIn(".msprompt", str(caught.exception))
        self.assertEqual(self.admitted, [])


class LibrarySurfaceTests(_CacheCase):
    def test_status_keys(self) -> None:
        status = self.service.status(self._params())
        self.assertEqual((status["prompt_cached"], status["prompt_cache_entry"]), (False, None))
        for params in ({"prompt": ""}, {"source_root": None}):
            with self.subTest(params=params):
                status = self.service.status(self._params(**params))
                self.assertEqual((status["prompt_cached"], status["prompt_cache_entry"]), (False, None))
        self.service.prompt_cache_build(self._params(), name="Кот")
        # The build's own encode stays in memory, which ranks before the file (FLUX.2's rule).
        status = self.service.status(self._params())
        self.assertEqual((status["prompt_cached"], status["prompt_cache_entry"]["origin"]), (True, "memory"))
        fresh = service.Flux3KleinService(self.rig.manager)
        entry = fresh.status(self._params())["prompt_cache_entry"]
        self.assertEqual((entry["name"], entry["encoder_source"], entry["encoder_format"], entry["current_encoder"], entry["origin"]), ("Кот", "stock", "bf16", True, "built"))

    def test_build_rename_and_already(self) -> None:
        built = self.service.prompt_cache_build(self._params(), name="Кот")
        # A new preset is written under its name: nothing to rename.
        self.assertEqual((built["prompt"], built["name"], built["encoded"], built["renamed"], built["prompt_cached"], built["device"]), ("a cat", "Кот", True, False, True, "cpu"))
        [written] = built["written"]
        self.assertEqual((written["encoder_format"], written["current_encoder"]), ("bf16", True))
        self.assertEqual([phase for phase, event, _info in self.recorder.events], [])
        again = self.service.prompt_cache_build(self._params())
        self.assertEqual((again["name"], again["encoded"], again["written"], again["renamed"]), ("Кот", False, [], False))
        renamed = self.service.prompt_cache_build(self._params(), name="Кошка")
        self.assertEqual((renamed["name"], renamed["encoded"], renamed["written"], renamed["renamed"]), ("Кошка", False, [], True))
        self.assertEqual(len(self.rig.encode_calls), 1)

    def test_build_reports_the_encoder_phases_and_releases_a_cold_encoder(self) -> None:
        recorder = Recorder()
        self.service.prompt_cache_build(self._params(), observer=recorder)
        self.assertEqual([(phase, event) for phase, event, _info in recorder.events], [(phase, event) for phase in (Phase.ENCODER_LOAD, Phase.ENCODE, Phase.ENCODER_RELEASE) for event in ("begin", "end")])
        self.assertEqual(recorder.begin(Phase.ENCODER_LOAD)["action"], "loaded")
        self.assertEqual(self.admitted, ["flux3_klein_text_encoder.text"], "the encoder handle alone")
        self.assertEqual(self.rig.manager.placements(self._encoder_key()), {}, "released after the build")

    def test_a_canceled_build_releases_the_encoder_and_is_no_error(self) -> None:
        def cancel_at_encode(phase: Phase, event: str, info: Any) -> None:
            if phase is Phase.ENCODE and event == "begin":
                raise service.Flux3Canceled("canceled")

        with self.assertRaises(service.Flux3Canceled):
            self.service.prompt_cache_build(self._params(), observer=cancel_at_encode)
        self.assertEqual(self.rig.manager.placements(self._encoder_key()), {}, "the read encoder is released")
        self.assertIsNone(self.service.health()["last_error"])
        self.assertEqual(self.rig.large_holdings()[1:], (0, 0))
        self.assertFalse(self.service.status(self._params())["prompt_cached"])

    def test_build_keeps_a_resident_encoder(self) -> None:
        options = self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP)
        self._run("another", options=options)
        self.assertTrue(self.rig.manager.placements(self._encoder_key(options)))
        self.service.prompt_cache_build(self._params(vram_step="max", ram_step="keep"))
        self.assertTrue(self.rig.manager.placements(self._encoder_key(options)))

    def test_build_from_memory_does_not_encode(self) -> None:
        self._run()
        built = self.service.prompt_cache_build(self._params())
        self.assertEqual((built["encoded"], len(built["written"])), (False, 1))
        self.assertEqual(len(self.rig.encode_calls), 1)

    def test_build_refusals(self) -> None:
        cases = (
            (self._params(prompt="  "), ValueError, "промпт пуст"),
            (self._params(encoder_mode="vl_refs"), ValueError, "encoder_mode"),
            (self._params(prompt_library=None), ValueError, "prompt_library"),
            (self._params(prompt_library="flux2_klein_9b"), ValueError, "prompt_library"),
        )
        for params, error, text in cases:
            with self.subTest(text=text), self.assertRaises(error) as caught:
                self.service.prompt_cache_build(params)
            self.assertIn(text, str(caught.exception))
        self._delete_encoder_weights()
        with self.assertRaises(FileNotFoundError):
            self.service.prompt_cache_build(self._params())
        self.assertEqual(self.admitted, [])

    def test_a_refused_build_encode_reads_nothing(self) -> None:
        self.rig.probes.host_available = 0
        self.rig.probes.device_total = 0
        with self.assertRaises(LargeModelRefused):
            self.service.prompt_cache_build(self._params())
        self.assertEqual(self.rig.encoders, [])

    def test_list_export_and_import(self) -> None:
        self.service.prompt_cache_build(self._params(), name="Кот")
        listing = self.service.prompt_cache_list(self._params())
        self.assertEqual((listing["library"], listing["text_encoder_available"], listing["legacy"], listing["skipped"]), (LIBRARY_ID, True, None, []))
        [row] = listing["texts"]
        self.assertEqual((row["prompt"], row["name"], row["selected"]["file"]), ("a cat", "Кот", row["entries"][0]["file"]))
        lenient = self.service.prompt_cache_list({"prompt_library": LIBRARY_ID})
        self.assertEqual((len(lenient["texts"]), lenient["text_encoder_available"], lenient["texts"][0]["selected"]["current_encoder"]), (1, False, False))
        with self.assertRaises(ValueError):
            self.service.prompt_cache_list({"source_root": str(self.rig.root)})
        out = self.program_root / "out" / "cat.msprompt"
        out.parent.mkdir()
        exported = self.service.prompt_cache_export(self._params(), row["selected"]["file"], str(out))
        self.assertEqual((exported["library"], exported["path"]), (LIBRARY_ID, str(out)))
        imported = self.service.prompt_cache_import(self._params(), str(out))
        self.assertEqual((imported["library"], imported["library_matches"], imported["already_present"], imported["name"]), (LIBRARY_ID, True, True, "Кот"))
        # A v2 file of another model's library is refused.
        foreign = out.with_name("foreign.msprompt")
        header, _start = pl.parse_payload_header(out.read_bytes())
        meta = {**header["__metadata__"], "library": "flux2_klein_9b"}
        foreign.write_bytes(pl.rewrite_safetensors_metadata(out.read_bytes(), meta))
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_import(self._params(), str(foreign))
        self.assertIn("flux2_klein_9b", str(caught.exception))
        # A v1 file (FLUX.2's protocol-22 container) is refused.
        v1 = out.with_name("v1.msprompt")
        save_file({"prompt_embeds": torch.zeros(1, 4, 16, dtype=torch.bfloat16)}, str(v1), metadata={"format": "manhwastudio.flux2_klein.prompt_cache", "format_version": "1"})
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self._params(), str(v1))

    def test_a_library_entry_serves_a_fresh_service(self) -> None:
        self.service.prompt_cache_build(self._params())
        self.service.unload()
        self.service = service.Flux3KleinService(self.rig.manager)
        self._run()
        self.assertFalse(self._encoder_admitted())
        self.assertEqual(len(self.rig.encode_calls), 1)
        # Read once from the file, then remembered: a library read is not repeated.
        with patch.object(pl.PromptLibrary, "read_tensor", side_effect=AssertionError("read twice")):
            self._run()


if __name__ == "__main__":
    unittest.main()
