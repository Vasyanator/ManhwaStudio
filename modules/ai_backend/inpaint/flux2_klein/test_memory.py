"""
File: modules/ai_backend/inpaint/flux2_klein/test_memory.py

Purpose:
Unit tests for `memory.py`, the FLUX.2 glue over the large-model manager: the
request's step pair (`params.vram_step` / `ram_step`, an absent axis «Авто») and
the four step keys reported back, the `forecast_memory` answer (shape, the
pipeline-then-encoder admission, the encoder on the card beside the pipeline
under (MAX, KEEP), the no-encoder case, the attention-mask row, `fits` on
unknown / tiny memory, `calibrated`, MIN uses less VRAM than MAX), the combined
VRAM-axis ceiling chain and its monotonicity for both handles together, «Энкодер
на CPU» (`EncoderOnCpuTests`: no VRAM step puts the encoder on the card, the RAM
step keeps or releases it), a
host run device (`cpu`: planned on the host alone, no VRAM figures), and a GGUF
transformer format (planned in the request's format; its unmeasured `gguf`
compute path makes the forecast an estimate, never an error).

Notes:
Header-only weight files (`_TempTreeCase`); the reference calibration table
is pinned so the numbers do not depend on the test host's GPU. No torch
needed, no GPU.
"""

from __future__ import annotations

import unittest
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import memory
from modules.ai_backend.runtime.large_models.cost_tables import REFERENCE_DEVICE_CLASS, load_tables
from modules.ai_backend.runtime.large_models.tiers import AUTO_STEPS, RamStep, Steps, VramStep

from ._test_fixtures import _TempTreeCase, _write_tiny_gguf
from .test_characterization import _golden_environment

GIB = 1024**3


MAX_KEEP = Steps(VramStep.MAX, RamStep.KEEP)
PHASE_KEEP = Steps(VramStep.PHASE, RamStep.KEEP)


class RequestedStepsTests(_TempTreeCase):
    def _steps(self, **overrides: object) -> Steps:
        return memory.requested_steps(svc.normalize_flux2_klein_params(self.params(**overrides)))

    def test_explicit_steps_and_the_absent_axis_is_auto(self) -> None:
        self.assertEqual(self._steps(vram_step="min", ram_step="min"), Steps(VramStep.MIN, RamStep.MIN))
        self.assertEqual(self._steps(vram_step="half"), Steps(VramStep.HALF, RamStep.AUTO))
        self.assertEqual(self._steps(ram_step="run"), Steps(VramStep.AUTO, RamStep.RUN))
        self.assertEqual(self._steps(vram_step="auto", ram_step="auto"), AUTO_STEPS)
        self.assertEqual(self._steps(), AUTO_STEPS)
        self.assertEqual(self._steps(vram_step=" ", ram_step=None), AUTO_STEPS)

    def test_the_step_keys_reported_back(self) -> None:
        fields = memory.steps_fields(Steps(VramStep.AUTO, RamStep.KEEP), Steps(VramStep.HALF, RamStep.KEEP))
        self.assertEqual(tuple(fields), memory.STEP_FIELD_KEYS)
        self.assertEqual(fields, {"vram_step": "auto", "ram_step": "keep", "vram_step_effective": "half", "ram_step_effective": "keep"})
        self.assertIsNone(memory.steps_fields(MAX_KEEP, None)["vram_step_effective"])

    def test_unknown_values_are_request_errors(self) -> None:
        for field, value in (("vram_step", "huge"), ("ram_step", "keep_forever")):
            with self.subTest(field=field), self.assertRaises(ValueError):
                svc.normalize_flux2_klein_params(self.params(**{field: value}))


class ForecastMemoryTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        reference = load_tables(REFERENCE_DEVICE_CLASS)
        tables = patch.object(memory, "_tables_for", lambda _device: reference)
        tables.start()
        self.addCleanup(tables.stop)

    def _forecast(self, memory_snapshot: dict[str, int] | None = None, **overrides: object) -> dict[str, object]:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return memory.forecast_memory(normalized, 512, 512, memory=memory_snapshot, device="cuda:0")

    def test_the_shape(self) -> None:
        out = self._forecast()
        self.assertEqual(set(out), {"vram_bytes", "ram_bytes", "phases", "resident", "breakdown", "fits", "calibrated", *memory.STEP_FIELD_KEYS})
        self.assertEqual(set(out["phases"]), {"encode", "encode_standalone", "denoise", "decode"})
        self.assertEqual(set(out["breakdown"]), {"transformer", "text_encoder", "vae", "activations", "peak_encode", "peak_denoise", "peak_decode"})
        self.assertEqual(out["vram_bytes"], max(phase["vram_bytes"] for phase in out["phases"].values()))
        self.assertTrue(out["calibrated"], "the reference table measures every FLUX.2 phase")
        self.assertTrue(out["fits"], "unknown memory is planned unbounded")

    def test_no_encoder_means_no_encode_phase(self) -> None:
        params = self.params()
        params.pop("text_encoder_path")
        out = memory.forecast_memory(svc.normalize_flux2_klein_params(params), 512, 512, device="cuda:0")
        self.assertEqual(out["phases"]["encode"], {"vram_bytes": 0, "ram_bytes": 0})
        self.assertEqual(out["phases"]["encode_standalone"], {"vram_bytes": 0, "ram_bytes": 0})
        self.assertEqual(out["breakdown"]["text_encoder"], 0)

    def test_the_attention_mask_is_a_row_only_while_it_costs(self) -> None:
        self.assertIn("attention_mask", self._forecast(text_attention_in_mask=True)["breakdown"])
        self.assertNotIn("attention_mask", self._forecast(text_attention_in_mask=True, whole_region=True)["breakdown"])

    def test_tiny_free_memory_does_not_fit(self) -> None:
        tiny = {"vram_total": 1 << 20, "vram_free": 1 << 10, "ram_total": 1 << 20, "ram_free": 1 << 10}
        self.assertFalse(self._forecast(tiny)["fits"])

    def test_the_encoder_falls_back_to_the_cpu_beside_the_pipeline(self) -> None:
        # Room for the pipeline but not for the encoder beside it: (MAX, KEEP)'s
        # encoder goes to the CPU and the encode phase then costs host memory.
        # The card is derived from the unbounded plan (pipeline peak + 3/4 of the
        # encoder's GPU peak), so the scenario survives a recalibration.
        roomy = self._forecast(vram_step="max", ram_step="keep", encoder_on_cpu=False)
        gpu_encoder = roomy["phases"]["encode_standalone"]["vram_bytes"]
        self.assertEqual(roomy["phases"]["encode_standalone"]["ram_bytes"], 0, "unbounded: the encoder runs on the card")
        card = max(roomy["phases"]["denoise"]["vram_bytes"], roomy["phases"]["decode"]["vram_bytes"]) + gpu_encoder * 3 // 4
        out = self._forecast({"vram_total": card, "vram_free": card, "ram_total": 64 * GIB, "ram_free": 40 * GIB}, vram_step="max", ram_step="keep", encoder_on_cpu=False)
        self.assertTrue(out["fits"])
        self.assertEqual((out["vram_step_effective"], out["ram_step_effective"]), ("max", "keep"))
        self.assertLess(out["phases"]["encode_standalone"]["vram_bytes"], gpu_encoder)
        self.assertGreater(out["phases"]["encode_standalone"]["ram_bytes"], 0)


def _has_gguf() -> bool:
    try:
        import gguf  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_gguf(), "needs the gguf package (fixture writer)")
class GgufFormatForecastTests(_TempTreeCase):
    """`forecast_memory` plans the request's transformer format (`large_model.weight_format_of`)."""

    def setUp(self) -> None:
        super().setUp()
        reference = load_tables(REFERENCE_DEVICE_CLASS)
        tables = patch.object(memory, "_tables_for", lambda _device: reference)
        tables.start()
        self.addCleanup(tables.stop)
        self.gguf = str(_write_tiny_gguf(self.root / "transformer_gguf" / "klein-q8.gguf"))
        self.formats = [{"id": "bf16", "path": self.paths["transformer_path"]}, {"id": "q8_0", "path": self.gguf}]

    def _forecast(self, **overrides: object) -> dict[str, object]:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return memory.forecast_memory(normalized, 512, 512, device="cuda:0")

    def test_a_gguf_transformer_is_forecast_as_an_estimate(self) -> None:
        out = self._forecast(transformer_path=self.gguf, transformer_format="q8_0", transformer_formats=self.formats)
        self.assertTrue(out["fits"])
        self.assertFalse(out["calibrated"], "no table measures the gguf compute path: the catch-all estimate")
        bf16 = self._forecast(transformer_format="bf16", transformer_formats=self.formats)
        self.assertTrue(bf16["calibrated"], "the measured bf16 denoise entry still applies to the bf16 candidate")
        self.assertNotEqual(out["breakdown"]["transformer"], bf16["breakdown"]["transformer"], "each format is planned on its own file")


class HostRunDeviceTests(_TempTreeCase):
    """A `cpu` run device is planned on the host alone, as the manager plans it (no unbounded-VRAM rule)."""

    def setUp(self) -> None:
        super().setUp()
        reference = load_tables(REFERENCE_DEVICE_CLASS)
        tables = patch.object(memory, "_tables_for", lambda _device: reference)
        tables.start()
        self.addCleanup(tables.stop)

    def _forecast(self, snapshot: dict[str, int], **overrides: object) -> dict[str, object]:
        return memory.forecast_memory(svc.normalize_flux2_klein_params(self.params(**overrides)), 512, 512, memory=snapshot, device="cpu")

    def test_the_predicate(self) -> None:
        self.assertTrue(memory.is_host_run_device("cpu"))
        self.assertTrue(memory.is_host_run_device("mps"))
        self.assertFalse(memory.is_host_run_device("cuda:0"))

    def test_everything_is_host_memory_and_the_ram_step_picks_the_plan(self) -> None:
        out = self._forecast({"vram_total": 0, "vram_free": 0, "ram_total": 64 * GIB, "ram_free": 60 * GIB}, vram_step="min", ram_step="keep")
        self.assertTrue(out["fits"])
        self.assertEqual(out["vram_bytes"], 0)
        self.assertTrue(all(phase["vram_bytes"] == 0 for phase in out["phases"].values()))
        self.assertGreater(out["ram_bytes"], 0)
        self.assertEqual(out["ram_step_effective"], "keep")

    def test_unknown_vram_no_longer_means_fits_when_ram_is_short(self) -> None:
        # The cuda rule "unknown VRAM = unbounded" must not apply: the host room alone judges.
        out = self._forecast({"vram_total": 0, "vram_free": 0, "ram_total": 1 << 20, "ram_free": 1 << 10})
        self.assertFalse(out["fits"])
        self.assertEqual(out["vram_bytes"], 0)


class StepMonotonicityTests(unittest.TestCase):
    """The klein 9B sizes (`test_characterization._golden_environment`) on an empty 32 GiB card."""

    CARD = {"vram_total": 32 * GIB, "vram_free": 31 * GIB, "ram_total": 128 * GIB, "ram_free": 100 * GIB}

    def test_a_lower_vram_step_never_forecasts_more_vram(self) -> None:
        # The user's complaint: the most economical setting used 27 GiB of an
        # empty 32 GiB card. MIN must stay far below MAX, and every step at or
        # below the one above it (same RAM step).
        with _golden_environment() as paths:
            for ram in (RamStep.KEEP, RamStep.RUN, RamStep.MIN):
                with self.subTest(ram=ram):
                    peaks = []
                    for vram in (VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN):
                        normalized = svc.normalize_flux2_klein_params({**paths, "vram_step": vram.value, "ram_step": ram.value})
                        out = memory.forecast_memory(normalized, 512, 512, memory=self.CARD, device="cuda:0")
                        self.assertTrue(out["fits"], vram)
                        self.assertEqual((out["vram_step_effective"], out["ram_step_effective"]), (vram.value, ram.value))
                        peaks.append(out["vram_bytes"])
                    self.assertEqual(peaks, sorted(peaks, reverse=True))
                    self.assertLess(peaks[-1], peaks[0] / 2)


class CombinedCeilingTests(unittest.TestCase):
    """`memory.combined_ceilings` / `encoder_device_cap`: the chain of the combined VRAM-axis rule (module notes)."""

    def test_each_step_is_capped_by_the_nearest_fitting_higher_step(self) -> None:
        planned: list[tuple[Steps, int | None]] = []
        peaks = {VramStep.MAX: 30, VramStep.PHASE: None, VramStep.HALF: 20}

        def combined_at(cell: Steps, ceiling: int | None) -> int | None:
            planned.append((cell, ceiling))
            return peaks[cell.vram]

        ceilings = memory.combined_ceilings(RamStep.RUN, combined_at, until=VramStep.MIN)
        self.assertEqual(ceilings, {VramStep.MAX: None, VramStep.PHASE: 30, VramStep.HALF: 30, VramStep.MIN: 20})
        # Each cell is planned under its own ceiling; the refused PHASE passes MAX's on; `until` itself is not planned.
        self.assertEqual(planned, [(Steps(VramStep.MAX, RamStep.RUN), None), (Steps(VramStep.PHASE, RamStep.RUN), 30), (Steps(VramStep.HALF, RamStep.RUN), 30)])

    def test_max_plans_nothing(self) -> None:
        self.assertEqual(memory.combined_ceilings(RamStep.KEEP, lambda _cell, _ceiling: self.fail("planned"), until=VramStep.MAX), {VramStep.MAX: None})


class EncoderOnCpuTests(unittest.TestCase):
    """«Энкодер на CPU» (`encoder_on_cpu`, the encoder spec's host pin) at the klein 9B sizes on an empty 32 GiB card.

    Every VRAM step's device forecast excludes the encoder (its standalone
    encode puts only the runtime overhead on the card and the encode moment is
    the pipeline's resting weights plus that), while the RAM step still
    governs it: kept in RAM between runs under KEEP, not under RUN / MIN.
    """

    CARD = {"vram_total": 32 * GIB, "vram_free": 31 * GIB, "ram_total": 128 * GIB, "ram_free": 100 * GIB}

    def test_every_vram_step_excludes_the_encoder_and_the_ram_step_places_it(self) -> None:
        tables = load_tables(REFERENCE_DEVICE_CLASS)
        overhead = tables.device.runtime_overhead_bytes
        with _golden_environment() as paths, patch.object(memory, "_tables_for", lambda _device: tables):
            for ram in ("keep", "run", "min"):
                for vram in ("max", "phase", "half", "min"):
                    with self.subTest(vram=vram, ram=ram):
                        pinned = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "vram_step": vram, "ram_step": ram}), 512, 512, memory=self.CARD, device="cuda:0")
                        self.assertTrue(pinned["fits"])
                        self.assertEqual((pinned["vram_step_effective"], pinned["ram_step_effective"]), (vram, ram))
                        self.assertEqual(pinned["phases"]["encode_standalone"]["vram_bytes"], overhead)
                        self.assertGreater(pinned["phases"]["encode_standalone"]["ram_bytes"], pinned["breakdown"]["text_encoder"])
                        self.assertEqual(pinned["phases"]["encode"]["vram_bytes"], pinned["resident"]["pipeline_device"] + overhead)
                        kept = pinned["resident"]["text_encoder_host"]
                        if ram == "keep":
                            self.assertGreaterEqual(kept, pinned["breakdown"]["text_encoder"])
                        else:
                            self.assertEqual(kept, 0)
            unpinned = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "vram_step": "max", "ram_step": "keep", "encoder_on_cpu": False}), 512, 512, memory=self.CARD, device="cuda:0")
            pinned = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "vram_step": "max", "ram_step": "keep"}), 512, 512, memory=self.CARD, device="cuda:0")
        self.assertLess(pinned["vram_bytes"], unpinned["vram_bytes"])


class EncoderOnTheCardTests(unittest.TestCase):
    """The klein 9B sizes on an empty 32 GiB card: (MAX, KEEP) puts the encoder beside the pipeline on the card."""

    CARD = {"vram_total": 32 * GIB, "vram_free": 31 * GIB, "ram_total": 128 * GIB, "ram_free": 100 * GIB}

    def test_max_keep_encodes_on_the_card(self) -> None:
        with _golden_environment() as paths:
            out = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "vram_step": "max", "ram_step": "keep", "encoder_on_cpu": False}), 512, 512, memory=self.CARD, device="cuda:0")
        self.assertTrue(out["fits"])
        self.assertEqual((out["vram_step_effective"], out["ram_step_effective"]), ("max", "keep"))
        # Most of the 9B encoder sits beside the pipeline on the card (as much as
        # the room left by the pipeline's reservation holds), so the encode moment
        # raises the device peak above the pipeline's own phases.
        half_encoder = out["breakdown"]["text_encoder"] // 2
        self.assertGreater(out["phases"]["encode_standalone"]["vram_bytes"], half_encoder)
        self.assertGreater(out["vram_bytes"], max(out["phases"]["denoise"]["vram_bytes"], out["phases"]["decode"]["vram_bytes"]))


if __name__ == "__main__":
    unittest.main()
