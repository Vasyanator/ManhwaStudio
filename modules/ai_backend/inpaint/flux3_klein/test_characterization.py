"""
File: modules/ai_backend/inpaint/flux3_klein/test_characterization.py

Purpose:
Characterization tests of `Flux3KleinService`, written BEFORE its encoder
policies and whole-DiT moves were replaced by the large-model manager (plan
WP5), and kept as the reference that refactor is checked against.

Every test class belongs to exactly ONE of two kinds, named in its docstring:

- CONTRACT - what WP5 keeps: the `Phase` list (the VRAM script's measurement
  contract), every phase reported exactly once as a begin/end pair in run order
  for every option set, the `end` event's `failed` flag, the observer `info`
  keys that do not depend on the removed options, the `health()` keys and their
  idle / after-run values, and the lease always being resolved. The
  assertions are the pre-WP5 ones; only the harness changed with the
  constructor (the service now runs under the real `LargeModelManager`, with
  stubs at the meta-build seams instead of the old whole loaders), and the
  lease is observed in the large pool's counters instead of the small pool's.
- GOLDEN - the CHOICES of the options that drive residency. Pre-WP5 that was
  `EncoderPolicy` x `dit_offload_during_ae`; behaviour change B5 removed both,
  so this class was REWRITTEN deliberately against the resource vocabulary,
  and re-derived again (by hand from `tiers.template_for`, then checked) when
  the single profile scale became the two axes `Flux3Options.vram_step` x
  `ram_step`, and once more when FLUX.3 split into a pipeline handle and a
  text-encoder handle (B3; the changed cells are named in the class): the
  `action` of each move phase per step pair over two calls, the info key set
  (the removed keys gone, the step / forecast keys added) and the applied
  fields.

Main responsibilities:
- `PhaseListContract`, `ObserverSequenceContract`, `HealthContract`,
  `LeaseContract` - the contracts;
- `StepActionsGolden` - the golden table.

Notes:
- No weights, no GPU, run device `cpu`: `_service_test_fixtures.ServiceRig`
  (tiny real safetensors of stand-in modules, fake budget probes, the real
  manager mechanics; the real `packing` / `sampling` code runs on tiny CPU
  tensors).
- Already pinned elsewhere and not repeated: the failure paths and the
  manager wiring (`test_service.py`), the spec (`test_large_model.py`).
"""

from __future__ import annotations

import unittest
from typing import Any

from PIL import Image

from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from . import service
from ._service_test_fixtures import TEXT_LEN as _TEXT_LEN
from ._service_test_fixtures import TEXT_REAL as _TEXT_REAL
from ._service_test_fixtures import Recorder, ServiceRig
from .config import WeightFormat
from .params import MIN_REFERENCE_PIXELS, AeDtype, EncoderMode, Flux3Options, Flux3Request, Fp8Compute, default_seed
from .phases import Phase

#: The observer `info` keys every phase carries that do NOT depend on the
#: options B5 removes. CONTRACT. `text_encoder_format` (the encoder's format id,
#: `bf16` or a GGUF id) joined with the GGUF encoder: a measurement row must say
#: which encoder ran, because the encode cost differs per format.
_STABLE_INFO_KEYS = {
    "variant",
    "text_encoder_format",
    "encoder_mode",
    "fp8_compute",
    "ae_dtype",
    "device",
    "reference_max_pixels",
    "n_refs",
    "width",
    "height",
    "steps",
    "seed",
    "image_tokens",
    "ref_tokens",
}

#: Phases that carry `action`, with the vocabulary each may report. CONTRACT for
#: the load phases; the move phases' vocabulary is listed for completeness and is
#: the part B5 may narrow (their VALUES are golden).
_ACTION_VOCABULARY = {
    Phase.DIT_LOAD: {"loaded", "resident"},
    Phase.AE_LOAD: {"loaded", "resident"},
    # `cached`: a caption served by the prompt cache (B4) opens no encoder; the
    # rig's encoder is unidentifiable (no `text_encoder/config.json`), so these
    # contracts never see it — `test_prompt_cache.py` pins that sequence.
    Phase.ENCODER_LOAD: {"loaded", "restored", "resident", "cached"},
    Phase.DIT_OFFLOAD: {"offloaded", "on_host", "skipped"},
    Phase.DIT_RESTORE: {"restored", "resident"},
    Phase.DIT_OFFLOAD_DECODE: {"offloaded", "on_host", "skipped"},
}

#: Phases that report the DiT text length (they run after the encode). CONTRACT.
_TEXT_TOKEN_PHASES = {
    Phase.ENCODER_RELEASE,
    Phase.REF_ENCODE,
    Phase.DIT_RESTORE,
    Phase.DENOISE,
    Phase.DIT_OFFLOAD_DECODE,
    Phase.DECODE,
}


class _ServiceCase(unittest.TestCase):
    """A service over the `ServiceRig`; records observer events. Carries no tests.

    `fail_ae_load` makes the AE load raise (a failed LOAD); `self.dits[-1].fail`
    makes the next denoise raise (a failed RUN).
    """

    def setUp(self) -> None:
        self.rig = ServiceRig(self)
        self.root = self.rig.root
        self.service = self.rig.service
        self.recorder = Recorder()

    @property
    def events(self) -> list[tuple[Phase, str, dict[str, Any]]]:
        return self.recorder.events

    @property
    def dits(self) -> list[Any]:
        return self.rig.dits

    @property
    def fail_ae_load(self) -> bool:
        return self.rig.fail_ae_build

    @fail_ae_load.setter
    def fail_ae_load(self, value: bool) -> None:
        self.rig.fail_ae_build = value

    def _options(self, **overrides: Any) -> Flux3Options:
        return Flux3Options(model_root=self.root, variant=WeightFormat.FP8R, **overrides)

    def _run(self, options: Flux3Options, request: Flux3Request | None = None) -> service.Flux3Result:
        self.recorder.events.clear()
        return self.service.generate(request or Flux3Request(caption="a cat"), options, observer=self.recorder)

    def _begin(self, phase: Phase) -> dict[str, Any]:
        return self.recorder.begin(phase)


def _reference(side: int = 64) -> Image.Image:
    return Image.new("RGB", (side, side), (10, 20, 30))


# ---------------------------------------------------------------------------
# CONTRACTS
# ---------------------------------------------------------------------------
class PhaseListContract(unittest.TestCase):
    """CONTRACT: the phase names, values and run order (VRAM script measurement contract)."""

    def test_the_phases_in_run_order(self) -> None:
        self.assertEqual(
            [(phase.name, phase.value) for phase in Phase],
            [
                ("DIT_LOAD", "dit_load"),
                ("AE_LOAD", "ae_load"),
                ("DIT_OFFLOAD", "dit_offload"),
                ("ENCODER_LOAD", "encoder_load"),
                ("ENCODE", "encode"),
                ("ENCODER_RELEASE", "encoder_release"),
                ("REF_ENCODE", "ref_encode"),
                ("DIT_RESTORE", "dit_restore"),
                ("DENOISE", "denoise"),
                ("DIT_OFFLOAD_DECODE", "dit_offload_decode"),
                ("DECODE", "decode"),
            ],
        )


class ObserverSequenceContract(_ServiceCase):
    """CONTRACT: every phase once, begin/end, in run order, for every surviving option set.

    The option sets touch only what survives B5: encoder mode, references and
    their cap, AE dtype, an explicit output size, and a second (resident) call.
    """

    #: name -> (options overrides, request, expected (width, height, n_refs)).
    CASES: dict[str, tuple[dict[str, Any], Flux3Request, tuple[int, int, int]]] = {
        "text_no_refs": ({}, Flux3Request(caption="a cat"), (64, 64, 0)),
        "text_one_ref": ({}, Flux3Request(caption="edit", references=(_reference(),)), (64, 64, 1)),
        "text_two_refs": ({}, Flux3Request(caption="mix", references=(_reference(), _reference(32))), (64, 64, 2)),
        "vl_refs_one_ref": (
            {"encoder_mode": EncoderMode.VL_REFS},
            Flux3Request(caption="edit", references=(_reference(),)),
            (64, 64, 1),
        ),
        "ae_bfloat16": ({"ae_dtype": AeDtype.BFLOAT16}, Flux3Request(caption="a cat"), (64, 64, 0)),
        # The cap shrinks the 128 px reference to 64 px BEFORE the default output
        # size is derived from it.
        "reference_cap": (
            {"reference_max_pixels": MIN_REFERENCE_PIXELS},
            Flux3Request(caption="edit", references=(_reference(128),)),
            (64, 64, 1),
        ),
        "explicit_size": ({}, Flux3Request(caption="a cat", width=96, height=64), (96, 64, 0)),
    }

    def _assert_sequence(self, case: str, expected_size: tuple[int, int, int]) -> None:
        self.assertEqual(
            [(phase, event) for phase, event, _info in self.events],
            [(phase, event) for phase in Phase for event in ("begin", "end")],
            case,
        )
        begins = {phase: info for phase, event, info in self.events if event == "begin"}
        for phase, event, info in self.events:
            if event == "end":
                # `end` repeats `begin`'s info and adds exactly `failed`.
                self.assertEqual(info, {**begins[phase], "failed": False}, (case, phase))
        for phase, info in begins.items():
            self.assertLessEqual(_STABLE_INFO_KEYS, set(info), (case, phase))
            self.assertEqual((info["width"], info["height"], info["n_refs"]), expected_size, (case, phase))
            self.assertEqual(info["device"], "cpu")
            if phase in _ACTION_VOCABULARY:
                self.assertIn(info["action"], _ACTION_VOCABULARY[phase], (case, phase))
            else:
                self.assertNotIn("action", info, (case, phase))
            if phase in _TEXT_TOKEN_PHASES:
                self.assertEqual((info["text_tokens"], info["text_real_tokens"]), (_TEXT_LEN, _TEXT_REAL))
            else:
                self.assertNotIn("text_tokens", info, (case, phase))

    def test_every_option_set_reports_the_full_sequence(self) -> None:
        for case, (overrides, request, expected) in self.CASES.items():
            with self.subTest(case=case):
                options = self._options(**overrides)
                result = self._run(options, request)
                self._assert_sequence(case, expected)
                self.assertEqual((result.width, result.height), expected[:2])
                self.assertEqual(self._begin(Phase.DENOISE)["seed"], default_seed(request.caption))
                self.assertEqual(self._begin(Phase.DENOISE)["encoder_mode"], options.encoder_mode.value)
                self.assertEqual(self._begin(Phase.DENOISE)["ae_dtype"], options.ae_dtype.value)
                ref_tokens = sum((image.width // 16) * (image.height // 16) for image in request.references)
                if case == "reference_cap":
                    ref_tokens = 16
                self.assertEqual(self._begin(Phase.DENOISE)["ref_tokens"], ref_tokens)
                self.assertEqual(
                    self._begin(Phase.DENOISE)["image_tokens"], (expected[0] // 16) * (expected[1] // 16)
                )

    def test_a_second_call_reports_the_same_sequence_with_resident_loads(self) -> None:
        self._run(self._options())
        self._run(self._options())
        self._assert_sequence("second call", (64, 64, 0))
        self.assertEqual(self._begin(Phase.DIT_LOAD)["action"], "resident")
        self.assertEqual(self._begin(Phase.AE_LOAD)["action"], "resident")

    def test_a_failed_denoise_ends_its_phase_and_nothing_follows(self) -> None:
        self._run(self._options())
        self.dits[-1].fail = True
        with self.assertRaises(RuntimeError):
            self._run(self._options())
        self.assertEqual(self.events[-1][:2], (Phase.DENOISE, "end"))
        self.assertTrue(self.events[-1][2]["failed"])
        phases = [phase for phase, _event, _info in self.events]
        self.assertNotIn(Phase.DECODE, phases)


class HealthContract(_ServiceCase):
    """CONTRACT: the `health()` keys and their meaning idle, after a run, after `unload`.

    A SUBSET check (WP5 may add keys, e.g. the applied profile); the exact current
    set is pinned by `test_service.py::test_health_answers_while_the_lock_is_held`.
    """

    KEYS = {
        "loaded",
        "ready",
        "device",
        "model_key",
        "variant",
        "fp8_compute",
        "dit_device",
        "encoder_resident",
        "last_error",
    }

    def _health(self) -> dict[str, Any]:
        health = self.service.health()
        self.assertLessEqual(self.KEYS, set(health))
        return {key: health[key] for key in self.KEYS - {"device"}}

    def test_idle_after_a_run_and_after_unload(self) -> None:
        idle = {
            "loaded": False,
            "ready": False,
            "model_key": None,
            "variant": None,
            "fp8_compute": None,
            "dit_device": None,
            "encoder_resident": None,
            "last_error": None,
        }
        self.assertEqual(self._health(), idle)
        options = self._options()
        self._run(options)
        after = self._health()
        self.assertEqual(
            {key: after[key] for key in ("loaded", "ready", "variant", "fp8_compute", "dit_device", "last_error")},
            {
                "loaded": True,
                "ready": True,
                "variant": "fp8r",
                "fp8_compute": "dequant",
                "dit_device": "cpu",
                "last_error": None,
            },
        )
        self.assertEqual(after["model_key"], service._model_key(options, "cpu"))
        self.assertEqual(self.service.health()["device"], "cpu")
        self.assertTrue(self.service.unload())
        self.assertEqual(self._health(), idle)

    def test_a_failed_run_keeps_the_pair_and_records_the_error(self) -> None:
        self._run(self._options())
        self.dits[-1].fail = True
        with self.assertRaises(RuntimeError):
            self._run(self._options())
        health = self._health()
        self.assertEqual((health["ready"], health["last_error"]), (True, "denoise exploded"))


class LeaseContract(_ServiceCase):
    """CONTRACT: the lease is resolved on every path, and a refusal never takes one.

    Observable through the pool's counters only (the large pool's since WP5),
    so the exact `mark_loaded` / `mark_load_failed` call pattern is not pinned
    here. Since B3 a run holds TWO large-pool handles — the pipeline (DiT +
    AE) and the text encoder, each its own residency key — so a successful
    run leaves two resident holdings where it left one (the default steps
    keep both between runs).
    """

    def _assert_resolved(self, resident: int) -> None:
        resident_count, in_use, loading = self.rig.large_holdings()
        self.assertEqual((loading, in_use, resident_count), (0, 0, resident))

    def test_success_registers_the_pipeline_and_the_encoder(self) -> None:
        self._run(self._options())
        self._assert_resolved(2)
        self._run(self._options())
        self._assert_resolved(2)

    def test_a_failed_load_registers_nothing(self) -> None:
        self.fail_ae_load = True
        with self.assertRaises(ValueError):
            self._run(self._options())
        self._assert_resolved(0)
        self.assertFalse(self.service.health()["loaded"])

    def test_a_failed_run_stays_registered(self) -> None:
        self._run(self._options())
        self.dits[-1].fail = True
        with self.assertRaises(RuntimeError):
            self._run(self._options())
        self._assert_resolved(2)

    def test_a_refusal_before_the_lease_touches_nothing(self) -> None:
        with self.assertRaises(ValueError):
            self._run(self._options(encoder_mode=EncoderMode.VL_REFS))
        self.assertEqual(self.events, [])
        self._assert_resolved(0)


# ---------------------------------------------------------------------------
# GOLDEN
# ---------------------------------------------------------------------------
class StepActionsGolden(_ServiceCase):
    """GOLDEN: the move phases' `action` per resource step pair (ample budgets).

    Rewritten in WP5 (B5) from the `EncoderPolicy` x `dit_offload_during_ae`
    table and re-derived for the two axes; a change here is a change of what a
    step pair does, to be reviewed. Under ample budgets «Не больше половины»
    plans like «Только текущий этап» (half of a huge card holds everything).

    Re-derived for the two handles (B3): the pipeline's first phase is
    `ref_encode`, which uses the AE, so where nothing is kept between runs
    (PHASE / HALF / MIN VRAM with RAM RUN or MIN) AE_LOAD now READS the AE
    into its `ref_encode` placement in the load scope (`loaded`), where it
    sits while the encoder runs; before the split the AE was kept nowhere at
    `encode` and was read by the encode->ref_encode transition (`resident`
    here). Those twelve AE_LOAD cells (both calls of the six pairs) are the
    only change; every other action and the after-run `health()` are as
    before (the encoder's own `finish` at ENCODER_RELEASE leaves it where the
    pair's between-runs state keeps it).
    """

    #: (vram, ram) -> two calls, each the `action` of (DIT_LOAD, AE_LOAD,
    #: DIT_OFFLOAD, ENCODER_LOAD, DIT_RESTORE, DIT_OFFLOAD_DECODE), then
    #: `health()` (`dit_device`, `encoder_resident`) after the second call
    #: (the run device is `cpu`, so "on the card" and "in RAM" both read `cpu`).
    _ON_CARD_KEPT = (
        ("loaded", "loaded", "skipped", "loaded", "resident", "skipped"),
        ("resident", "resident", "skipped", "resident", "resident", "skipped"),
        ("cpu", "cpu"),
    )
    _PHASE_KEPT_IN_RAM = (
        ("loaded", "loaded", "on_host", "loaded", "restored", "offloaded"),
        ("resident", "resident", "on_host", "restored", "restored", "offloaded"),
        ("cpu", "cpu"),
    )
    _PHASE_DROPPED = (
        ("resident", "loaded", "skipped", "loaded", "restored", "offloaded"),
        ("resident", "loaded", "skipped", "loaded", "restored", "offloaded"),
        (None, None),
    )
    TABLE: dict[Steps, tuple[tuple[str, ...], tuple[str, ...], tuple[Any, Any]]] = {
        # «Максимум скорости»: everything on the card, kept between runs, whatever the RAM step.
        Steps(VramStep.MAX, RamStep.KEEP): _ON_CARD_KEPT,
        Steps(VramStep.MAX, RamStep.RUN): _ON_CARD_KEPT,
        Steps(VramStep.MAX, RamStep.MIN): _ON_CARD_KEPT,
        # The card holds the current phase's component; the others parked in
        # RAM, also between runs (the encoder restored from RAM on call 2).
        Steps(VramStep.PHASE, RamStep.KEEP): _PHASE_KEPT_IN_RAM,
        Steps(VramStep.HALF, RamStep.KEEP): _PHASE_KEPT_IN_RAM,
        # Nothing kept between runs: before its first use a component holds
        # nothing (read by the step that needs it; the AE by the load scope,
        # its first use being the pipeline's first phase), parked in RAM (RUN)
        # or dropped (MIN) after it.
        Steps(VramStep.PHASE, RamStep.RUN): _PHASE_DROPPED,
        Steps(VramStep.HALF, RamStep.RUN): _PHASE_DROPPED,
        Steps(VramStep.PHASE, RamStep.MIN): _PHASE_DROPPED,
        Steps(VramStep.HALF, RamStep.MIN): _PHASE_DROPPED,
        # «Минимум» VRAM: no DiT block on the card (k = 0, juggled from RAM, so
        # it never "leaves the card"); the encoder runs on the CPU.
        Steps(VramStep.MIN, RamStep.KEEP): (
            ("loaded", "loaded", "on_host", "loaded", "restored", "on_host"),
            ("resident", "resident", "on_host", "resident", "restored", "on_host"),
            ("cpu", "cpu"),
        ),
        Steps(VramStep.MIN, RamStep.RUN): (
            ("resident", "loaded", "skipped", "loaded", "restored", "on_host"),
            ("resident", "loaded", "skipped", "loaded", "restored", "on_host"),
            (None, None),
        ),
        # Both «Минимум»: the DiT (and the encoder) streamed from the file with
        # k = 0; its non-block weights leave the card after the denoise.
        Steps(VramStep.MIN, RamStep.MIN): _PHASE_DROPPED,
    }

    _MOVE_PHASES = (
        Phase.DIT_LOAD,
        Phase.AE_LOAD,
        Phase.DIT_OFFLOAD,
        Phase.ENCODER_LOAD,
        Phase.DIT_RESTORE,
        Phase.DIT_OFFLOAD_DECODE,
    )

    def _actions(self) -> tuple[str, ...]:
        return tuple(self._begin(phase)["action"] for phase in self._MOVE_PHASES)

    def test_the_action_table(self) -> None:
        self.assertEqual(len(self.TABLE), 12)
        for steps, (first, second, after) in self.TABLE.items():
            with self.subTest(steps=steps.label):
                self.service.unload()
                options = self._options(vram_step=steps.vram, ram_step=steps.ram)
                self._run(options)
                self.assertEqual(self._actions(), first)
                decode = self._begin(Phase.DECODE)
                self.assertEqual((decode["vram_step_effective"], decode["ram_step_effective"]), (steps.vram.value, steps.ram.value))
                self._run(options)
                self.assertEqual(self._actions(), second)
                health = self.service.health()
                self.assertEqual((health["dit_device"], health["encoder_resident"]), after)

    def test_the_full_info_key_set(self) -> None:
        self._run(self._options())
        self.assertEqual(
            set(self._begin(Phase.DECODE)) - {"action", "text_tokens", "text_real_tokens"},
            _STABLE_INFO_KEYS
            | {"vram_step", "ram_step", "vram_step_effective", "ram_step_effective", "compute_path", "forecast_device_peak", "forecast_host_peak", "forecast_run_device_peak", "forecast_run_host_peak", "forecast_calibrated"},
        )

    def test_the_applied_fields(self) -> None:
        result = self._run(self._options())
        applied = result.applied
        self.assertEqual(
            (applied.variant, applied.fp8_compute, applied.encoder_mode, applied.ae_dtype, applied.compute_path, applied.steps_requested, applied.steps_effective, applied.degraded),
            (WeightFormat.FP8R, Fp8Compute.DEQUANT, EncoderMode.TEXT, AeDtype.FLOAT32, "dequant", Steps(VramStep.AUTO, RamStep.AUTO), Steps(VramStep.MAX, RamStep.KEEP), False),
        )


if __name__ == "__main__":
    unittest.main()
