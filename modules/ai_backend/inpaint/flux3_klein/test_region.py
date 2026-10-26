"""
File: modules/ai_backend/inpaint/flux3_klein/test_region.py

Purpose:
Unit tests of `region.py` over the real service and manager
(`_service_test_fixtures.ServiceRig`: stand-in models on the CPU, the stub AE
decodes flat grey): the answer is the RAW generated region (no colour match, no
composite: flat grey outside the mask too), `whole_region` (a solid mask
required, the whole region regenerated),
reference order (the region is #0), `seed: null` drawn and echoed, the
progress frames (stable `stage` ids), cancellation at a phase begin and at a
denoise step with no session left in use and a follow-up run that succeeds,
a generated image of the wrong size refused, and the pre-run refusals;
the disk-read frames (`DiskReadProgressTests`: one component-labelled `load`
frame stream per checkpoint read, in the stage that reads, at the manager's
cadence under an injected clock; re-reads labelled the same; the
`file_component` / `file_label` rules).

Notes:
CPU only; files go to temporary directories.
"""

from __future__ import annotations

import io
import unittest
from collections import defaultdict
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

from ...runtime import safetensors_stream
from ...runtime.large_models import moves
from ...runtime.large_models.loader import LoadProgress
from . import region, service
from ._service_test_fixtures import ServiceRig
from .config import WeightFormat
from .phases import Phase

SIZE = 128


def _png(array: np.ndarray, mode: str) -> bytes:
    with io.BytesIO() as buffer:
        Image.fromarray(array, mode).save(buffer, format="PNG")
        return buffer.getvalue()


def _region() -> np.ndarray:
    rng = np.random.default_rng(3)
    return rng.integers(30, 220, size=(SIZE, SIZE, 3), dtype=np.uint8)


def _mask(*, solid: bool = False) -> np.ndarray:
    mask = np.zeros((SIZE, SIZE), dtype=np.uint8)
    if solid:
        mask[:] = 255
    else:
        mask[32:96, 40:100] = 255
    return mask


class RegionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R,))
        self.region = _region()

    def _params(self, **overrides: Any) -> dict[str, Any]:
        return {"source_root": str(self.rig.root), "prompt": "remove the text", "steps": 2, **overrides}

    def _edit(self, *, mask: np.ndarray | None = None, references: tuple[bytes, ...] = (), progress: Any = None, cancel: Any = None, **params: Any) -> dict[str, Any]:
        return region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(_mask() if mask is None else mask, "L"), references, self._params(**params), progress_callback=progress, should_cancel=cancel)

    @staticmethod
    def _decoded(answer: dict[str, Any]) -> np.ndarray:
        with Image.open(io.BytesIO(answer["image_png"])) as image:
            return np.asarray(image.convert("RGB"))

    def test_the_answer_is_the_raw_generated_region(self) -> None:
        # Protocol 17: the client composites. The stub decodes one flat grey,
        # so the raw answer is that grey everywhere, outside the mask too.
        answer = self._edit()
        out = self._decoded(answer)
        self.assertEqual(out.shape, self.region.shape)
        self.assertEqual(len(np.unique(out.reshape(-1, 3), axis=0)), 1)
        outside = _mask() == 0
        self.assertFalse(np.array_equal(out[outside], self.region[outside]))
        self.assertEqual((answer["width"], answer["height"], answer["steps"]), (SIZE, SIZE, 2))
        self.assertEqual(answer["applied"], {"format": "fp8r", "fp8_compute": "dequant", "compute_path": "dequant", "encoder_mode": "text", "ae_dtype": "float32"})
        self.assertEqual((answer["vram_step"], answer["ram_step"]), ("auto", "auto"))
        self.assertIsNotNone(answer["vram_step_effective"])
        self.assertFalse(answer["degraded"])
        self.assertIsNone(answer["degrade_reason"])

    def test_whole_region_needs_a_solid_mask_and_regenerates_everything(self) -> None:
        with self.assertRaises(ValueError):
            self._edit(whole_region=True)
        out = self._decoded(self._edit(mask=_mask(solid=True), whole_region=True))
        # The stub decodes one flat grey: every pixel is the generated one.
        self.assertEqual(len(np.unique(out.reshape(-1, 3), axis=0)), 1)
        self.assertFalse(np.array_equal(out, self.region))

    def test_an_empty_mask_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self._edit(mask=np.zeros((SIZE, SIZE), dtype=np.uint8))

    def test_the_region_is_reference_zero(self) -> None:
        extra = np.full((64, 64, 3), 7, dtype=np.uint8)
        seen: list[Any] = []
        original = service.Flux3KleinService.generate

        def record(this: Any, request: Any, options: Any, **kwargs: Any) -> Any:
            seen.append([image.size for image in request.references])
            seen.append(np.asarray(request.references[0]).copy())
            seen.append((request.width, request.height))
            return original(this, request, options, **kwargs)

        with patch.object(service.Flux3KleinService, "generate", record):
            self._edit(references=(_png(extra, "RGB"),))
        self.assertEqual(seen[0], [(SIZE, SIZE), (64, 64)])
        self.assertTrue(np.array_equal(seen[1], self.region))
        self.assertEqual(seen[2], (SIZE, SIZE))

    def test_a_null_seed_is_drawn_and_echoed(self) -> None:
        drawn = self._edit(seed=None)["seed"]
        self.assertIsInstance(drawn, int)
        self.assertTrue(0 <= drawn < 2**32)
        self.assertEqual(self._edit(seed=1234)["seed"], 1234)
        with patch.object(region.secrets, "randbits", lambda bits: 99):
            self.assertEqual(self._edit()["seed"], 99)

    def test_progress_frames_carry_stable_stages(self) -> None:
        frames: list[tuple[str, int, int, str]] = []

        def progress(phase: str, step: int, total: int, label: str, *, stage: str | None = None, **file: Any) -> None:
            # Step frames only: the disk-read frames have their own tests.
            if not file:
                frames.append((phase, step, total, stage))

        self._edit(progress=progress)
        stages = region.progress_stages()
        self.assertEqual(frames[0], ("load", 0, len(stages) - 1, "prepare"))
        load_stages = [stage for phase, _, _, stage in frames if phase == "load"]
        self.assertEqual(load_stages, [stage for stage in stages if stage != Phase.DENOISE.value])
        for phase, step, total, stage in frames:
            if phase == "load":
                self.assertEqual((step, total), (stages.index(stage), len(stages) - 1))
        generate = [(step, total) for phase, step, total, stage in frames if phase == "generate"]
        self.assertEqual(generate, [(0, 2), (1, 2), (2, 2)])
        self.assertEqual({stage for phase, _, _, stage in frames if phase == "generate"}, {"denoise"})

    def test_cancel_at_a_phase_begin_and_at_a_denoise_step(self) -> None:
        self._edit()
        warm = self.rig.dits[-1]
        # The cancel is checked BEFORE a phase's frame: "encode" cancels at the
        # ENCODE begin once the ENCODER_LOAD frame went out; "step" after the
        # first denoise step's frame.
        for when, after in (("before", None), ("encode", "encoder_load:"), ("step", "denoise:1")):
            with self.subTest(when=when):
                frames: list[str] = []

                def progress(phase: str, step: int, total: int, label: str, *, stage: str | None = None, **file: Any) -> None:
                    if not file:
                        frames.append(f"{stage}:{step}")

                def cancel() -> bool:
                    return after is None or any(frame.startswith(after) for frame in frames)

                with self.assertRaises(service.Flux3Canceled):
                    self._edit(progress=progress, cancel=cancel)
                self.assertEqual(self.rig.large_holdings()[1:], (0, 0), "no session left in use")
                self.assertIsNone(self.rig.service.health()["last_error"])
        answer = self._edit()
        self.assertIs(self.rig.dits[-1], warm, "a cancel unloaded nothing")
        self.assertEqual(answer["width"], SIZE)

    def test_a_wrong_output_size_is_refused(self) -> None:
        original = service.Flux3KleinService.generate

        def shrink(this: Any, request: Any, options: Any, **kwargs: Any) -> Any:
            from dataclasses import replace

            result = original(this, request, options, **kwargs)
            return replace(result, image=result.image.resize((SIZE // 2, SIZE)))

        with patch.object(service.Flux3KleinService, "generate", shrink), self.assertRaises(ValueError):
            self._edit()

    def test_refusals_before_the_run(self) -> None:
        cases = {
            "size": lambda: region.edit_region_bytes(self.rig.service, _png(self.region[:120], "RGB"), _png(_mask()[:120], "L"), (), self._params()),
            "mask_size": lambda: region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(_mask()[:64], "L"), (), self._params()),
            "mask_mode": lambda: region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(self.region, "RGB"), (), self._params()),
            "prompt": lambda: region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(_mask(), "L"), (), self._params(prompt="")),
            "too_many": lambda: self._edit(references=tuple(_png(self.region, "RGB") for _ in range(10))),
            "bad_reference": lambda: self._edit(references=(b"not a png",)),
            "empty_reference": lambda: self._edit(references=(b"",)),
        }
        for name, call in cases.items():
            with self.subTest(case=name):
                with self.assertRaises(ValueError):
                    call()
        self.assertEqual(self.rig.dits, [], "no refusal read a weight")


class _TickingClock:
    """An injected `ByteProgress` clock: every call advances `tick` seconds (exact in binary); `now` reads without advancing."""

    def __init__(self, tick: float) -> None:
        self.now = 0.0
        self.tick = tick

    def __call__(self) -> float:
        self.now += self.tick
        return self.now


class DiskReadProgressTests(unittest.TestCase):
    """`edit_region_bytes`'s disk-read frames over the real manager (stand-in models, tiny real checkpoints)."""

    #: The rate limit of every stream (`safetensors_stream.PROGRESS_MIN_INTERVAL_SECONDS`).
    INTERVAL = 0.2

    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R,))
        self.region = _region()
        self.clock = _TickingClock(0.125)
        clock = self.clock

        class ClockedProgress(safetensors_stream.ByteProgress):
            def __init__(self, *args: Any, **kwargs: Any) -> None:
                super().__init__(*args, **{**kwargs, "clock": clock})

        # 16-byte read chunks: a 64-byte stand-in tensor is four reads, so one
        # stream spans several clock ticks and the rate limit is observable.
        for owner, name, value in ((safetensors_stream, "ByteProgress", ClockedProgress), (moves, "ByteProgress", ClockedProgress), (safetensors_stream, "READ_CHUNK_BYTES", 16)):
            patcher = patch.object(owner, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def _edit(self, **params: Any) -> tuple[list[dict[str, Any]], list[tuple[str, int, int, str | None]]]:
        """One run: (disk-read frames with the clock time they came at, step frames)."""
        reads: list[dict[str, Any]] = []
        steps: list[tuple[str, int, int, str | None]] = []

        def progress(phase: str, step: int, total: int, label: str, *, stage: str | None = None, **file: Any) -> None:
            if file:
                reads.append({"phase": phase, "step": step, "total": total, "label": label, "stage": stage, "at": self.clock.now, **file})
            else:
                steps.append((phase, step, total, stage))

        all_params = {"source_root": str(self.rig.root), "prompt": "remove the text", "steps": 2, **params}
        region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(_mask(), "L"), (), all_params, progress_callback=progress)
        return reads, steps

    def _assert_stream(self, frames: list[dict[str, Any]]) -> None:
        """One read: bytes grow to the total, 100% exactly once, frames at least one interval apart but the final one."""
        done = [frame["file_step"] for frame in frames]
        total = frames[0]["file_total"]
        self.assertEqual(done, sorted(done))
        self.assertEqual(done[-1], total)
        self.assertEqual(done.count(total), 1, "100% is reported exactly once")
        self.assertEqual({frame["file_total"] for frame in frames}, {total})
        times = [frame["at"] for frame in frames[:-1]]
        for earlier, later in zip(times, times[1:]):
            self.assertGreaterEqual(later - earlier, self.INTERVAL)

    def test_a_cold_run_reads_every_component_as_one_labelled_stream(self) -> None:
        reads, steps = self._edit()
        stages = region.progress_stages()
        by_component: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for frame in reads:
            by_component[frame["file_component"]].append(frame)
            # The frame of the stage in force, plus the byte level: the overall bar does not move.
            self.assertEqual(frame["phase"], "load")
            self.assertEqual((frame["step"], frame["total"]), (stages.index(frame["stage"]), len(stages) - 1))
            self.assertIn(("load", frame["step"], frame["total"], frame["stage"]), steps)
        self.assertEqual(set(by_component), {"transformer", "vae", "text_encoder"})
        expected = {
            "transformer": (Phase.DIT_LOAD.value, "model.safetensors"),
            "vae": (Phase.AE_LOAD.value, "ae.safetensors"),
            "text_encoder": (Phase.ENCODER_LOAD.value, "model-00001-of-00001.safetensors"),
        }
        for component, frames in by_component.items():
            with self.subTest(component=component):
                self.assertEqual({(frame["stage"], frame["file_label"]) for frame in frames}, {expected[component]})
                self._assert_stream(frames)
        # 192 bytes of DiT in 16-byte chunks, one tick each: several frames, not one.
        self.assertGreaterEqual(len(by_component["transformer"]), 3)

    def test_units_brought_back_from_disk_read_as_the_same_component(self) -> None:
        # The first run under VRAM «Минимум» x RAM «Минимум» drops what it does not
        # use; the second re-reads the DiT units on entering the denoise (`reread`).
        self._edit(vram_step="min", ram_step="min")
        with patch.object(region, "file_label", wraps=region.file_label) as labels:
            reads, _ = self._edit(vram_step="min", ram_step="min")
        rereads = [call.args[0] for call in labels.call_args_list if call.args[0].reread]
        self.assertTrue(rereads, "the run re-read units from disk")
        dit = [frame for frame in reads if frame["stage"] == Phase.DIT_RESTORE.value]
        self.assertTrue(dit)
        self.assertEqual({(frame["file_component"], frame["file_label"]) for frame in dit}, {("transformer", "model.safetensors")})
        self._assert_stream(dit)

    def test_no_callback_registers_no_sink(self) -> None:
        with patch.object(service.Flux3KleinService, "generate", autospec=True, side_effect=service.Flux3KleinService.generate) as generate:
            region.edit_region_bytes(self.rig.service, _png(self.region, "RGB"), _png(_mask(), "L"), (), {"source_root": str(self.rig.root), "prompt": "p", "steps": 1})
        self.assertIsNone(generate.call_args.kwargs["load_progress"])

    def test_component_ids_and_labels(self) -> None:
        self.assertEqual([region.file_component_id(name) for name in ("dit", "encoder", "ae", "other")], ["transformer", "text_encoder", "vae", "other"])

        def frame(*files: str, current: str | None = None) -> LoadProgress:
            paths = tuple(Path(name) for name in files)
            return LoadProgress(component="encoder", file=Path(current) if current else (paths[0] if paths else None), files=paths, done_bytes=0, total_bytes=0, reread=False)

        self.assertEqual(region.file_label(frame("/m/variants/fp8r/model.safetensors")), "model.safetensors")
        # Sharded in one folder: the folder, so the label does not change mid-bar.
        self.assertEqual(region.file_label(frame("/m/text_encoder/a.safetensors", "/m/text_encoder/b.safetensors", current="/m/text_encoder/b.safetensors")), "text_encoder")
        self.assertEqual(region.file_label(frame("/m/x/a.safetensors", "/m/y/b.safetensors", current="/m/y/b.safetensors")), "b.safetensors")
        self.assertEqual(region.file_label(frame()), "")


if __name__ == "__main__":
    unittest.main()
