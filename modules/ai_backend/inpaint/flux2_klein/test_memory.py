"""
File: modules/ai_backend/inpaint/flux2_klein/test_memory.py

Purpose:
Unit tests for `memory.py`, the stage-1 adapters over the large-model manager:
the legacy `placement` -> step pair mapping (also the stage-2 settings
migration), the `params.vram_step` / `ram_step` / legacy `profile` overrides and
the legacy names reported back, the legacy wire's CPU pin of the text encoder
(`encoder_cpu_pinned`, and the forecast keeping the encoder off the card under
it), the `forecast_memory` answer (shape, the pipeline-then-encoder admission,
the no-encoder case, the attention-mask row, `fits` on unknown / tiny memory,
`calibrated`, MIN uses less VRAM than MAX), the combined VRAM-axis ceiling
chain and its monotonicity for both handles together, a host run device (`cpu`: planned
on the host alone, no VRAM figures) and the step-pair advice.

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
from modules.ai_backend.runtime.large_models.tiers import AUTO_STEPS, STEP_WALK, RamStep, Steps, VramStep

from ._test_fixtures import _TempTreeCase
from .test_characterization import _golden_environment

GIB = 1024**3


MAX_KEEP = Steps(VramStep.MAX, RamStep.KEEP)
PHASE_KEEP = Steps(VramStep.PHASE, RamStep.KEEP)


class RequestedStepsTests(_TempTreeCase):
    def _steps(self, **overrides: object) -> Steps:
        return memory.requested_steps(svc.normalize_flux2_klein_params(self.params(**overrides)))

    def test_the_placement_mapping_table(self) -> None:
        self.assertEqual(self._steps(placement="full_gpu"), MAX_KEEP)
        self.assertEqual(self._steps(placement="full_gpu", unload_transformer_before_vae=True), MAX_KEEP)
        self.assertEqual(self._steps(placement="encoder_cpu", unload_transformer_before_vae=False), MAX_KEEP)
        self.assertEqual(self._steps(placement="encoder_cpu"), PHASE_KEEP, "its default parks the transformer")
        self.assertEqual(self._steps(placement="model_cpu_offload"), PHASE_KEEP)
        self.assertEqual(self._steps(placement="sequential_cpu_offload"), Steps(VramStep.MIN, RamStep.KEEP))

    def test_explicit_steps_win_and_the_absent_axis_is_auto(self) -> None:
        self.assertEqual(self._steps(placement="full_gpu", vram_step="min", ram_step="min"), Steps(VramStep.MIN, RamStep.MIN))
        self.assertEqual(self._steps(placement="full_gpu", vram_step="half"), Steps(VramStep.HALF, RamStep.AUTO))
        self.assertEqual(self._steps(placement="full_gpu", ram_step="run", profile="max_economy"), Steps(VramStep.AUTO, RamStep.RUN), "the steps beat a legacy profile")
        self.assertEqual(self._steps(vram_step="auto", ram_step="auto"), AUTO_STEPS)

    def test_the_legacy_profile_ids(self) -> None:
        self.assertEqual(set(memory.LEGACY_PROFILE_STEPS), set(svc.VALID_PROFILES))
        expected = {
            "auto": AUTO_STEPS,
            "max_speed": MAX_KEEP,
            "fast": MAX_KEEP,
            "balanced": PHASE_KEEP,
            "economy": Steps(VramStep.HALF, RamStep.RUN),
            "max_economy": Steps(VramStep.MIN, RamStep.MIN),
        }
        for profile, steps in expected.items():
            with self.subTest(profile=profile):
                self.assertEqual(self._steps(placement="full_gpu", profile=profile), steps)

    def test_the_legacy_names_reported_back(self) -> None:
        # Every id but `fast` (which shares MAX with `max_speed`) round-trips.
        for profile, steps in memory.LEGACY_PROFILE_STEPS.items():
            with self.subTest(profile=profile):
                self.assertEqual(memory.legacy_profile_name(steps), "max_speed" if profile == "fast" else profile)
        fields = memory.steps_fields(Steps(VramStep.AUTO, RamStep.KEEP), Steps(VramStep.HALF, RamStep.KEEP))
        self.assertEqual(tuple(fields), memory.STEP_FIELD_KEYS)
        self.assertEqual(fields, {"profile": "auto", "profile_effective": "economy", "vram_step": "auto", "ram_step": "keep", "vram_step_effective": "half", "ram_step_effective": "keep"})
        self.assertIsNone(memory.steps_fields(MAX_KEEP, None)["vram_step_effective"])

    def test_unknown_values_are_request_errors(self) -> None:
        for field, value in (("profile", "turbo"), ("vram_step", "huge"), ("ram_step", "keep_forever")):
            with self.subTest(field=field), self.assertRaises(ValueError):
                svc.normalize_flux2_klein_params(self.params(**{field: value}))

    def test_empty_values_mean_the_legacy_mapping(self) -> None:
        self.assertEqual(self._steps(placement="full_gpu", profile="", vram_step=" ", ram_step=None), MAX_KEEP)


class EncoderCpuPinTests(_TempTreeCase):
    def _pinned(self, **overrides: object) -> bool:
        return memory.encoder_cpu_pinned(svc.normalize_flux2_klein_params(self.params(**overrides)))

    def test_every_legacy_placement_pins_the_encoder(self) -> None:
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                self.assertTrue(self._pinned(placement=placement))

    def test_a_legacy_profile_pins_it_unless_its_ram_step_offers_no_cpu(self) -> None:
        for profile, steps in memory.LEGACY_PROFILE_STEPS.items():
            with self.subTest(profile=profile):
                self.assertEqual(self._pinned(placement="full_gpu", profile=profile), steps.ram is not RamStep.MIN)

    def test_explicit_steps_never_pin(self) -> None:
        self.assertFalse(self._pinned(placement="full_gpu", vram_step="max", ram_step="keep"))
        self.assertFalse(self._pinned(placement="encoder_cpu", vram_step="auto"))
        self.assertFalse(self._pinned(ram_step="keep", profile="balanced"))


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
        # unpinned encoder goes to the CPU and the encode phase then costs host memory.
        # The card is derived from the unbounded plan (pipeline peak + 3/4 of the
        # encoder's GPU peak), so the scenario survives a recalibration.
        roomy = self._forecast(vram_step="max", ram_step="keep")
        gpu_encoder = roomy["phases"]["encode_standalone"]["vram_bytes"]
        self.assertEqual(roomy["phases"]["encode_standalone"]["ram_bytes"], 0, "unbounded: the encoder runs on the card")
        card = max(roomy["phases"]["denoise"]["vram_bytes"], roomy["phases"]["decode"]["vram_bytes"]) + gpu_encoder * 3 // 4
        out = self._forecast({"vram_total": card, "vram_free": card, "ram_total": 64 * GIB, "ram_free": 40 * GIB}, vram_step="max", ram_step="keep")
        self.assertTrue(out["fits"])
        self.assertEqual((out["vram_step_effective"], out["ram_step_effective"], out["profile_effective"]), ("max", "keep", "max_speed"))
        self.assertLess(out["phases"]["encode_standalone"]["vram_bytes"], gpu_encoder)
        self.assertGreater(out["phases"]["encode_standalone"]["ram_bytes"], 0)


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


class LegacyEncoderOnHostTests(unittest.TestCase):
    """The klein 9B sizes on an empty 32 GiB card: `full_gpu` keeps the pipeline on the card, never the encoder."""

    CARD = {"vram_total": 32 * GIB, "vram_free": 31 * GIB, "ram_total": 128 * GIB, "ram_free": 100 * GIB}

    def test_full_gpu_encodes_on_the_host_while_the_same_explicit_pair_may_use_the_card(self) -> None:
        with _golden_environment() as paths:
            legacy = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "placement": "full_gpu"}), 512, 512, memory=self.CARD, device="cuda:0")
            explicit = memory.forecast_memory(svc.normalize_flux2_klein_params({**paths, "vram_step": "max", "ram_step": "keep"}), 512, 512, memory=self.CARD, device="cuda:0")
        self.assertEqual((legacy["vram_step_effective"], legacy["ram_step_effective"]), ("max", "keep"))
        self.assertTrue(legacy["fits"])
        # Pinned: no encoder weight on the card (the forecast's own per-phase device
        # overhead stays), the encoder resident in RAM.
        self.assertLess(legacy["phases"]["encode_standalone"]["vram_bytes"], GIB)
        self.assertGreater(legacy["phases"]["encode_standalone"]["ram_bytes"], 10 * GIB)
        self.assertGreater(legacy["resident"]["text_encoder_host"], 10 * GIB)
        # The pinned encoder adds nothing to the device peak: it is the pipeline's own.
        self.assertEqual(legacy["vram_bytes"], max(legacy["phases"]["denoise"]["vram_bytes"], legacy["phases"]["decode"]["vram_bytes"]))
        # Unpinned (MAX, KEEP) puts most of the 9B encoder beside the pipeline on
        # the card (as much as the room left by the pipeline's reservation holds).
        half_encoder = explicit["breakdown"]["text_encoder"] // 2
        self.assertGreater(explicit["phases"]["encode_standalone"]["vram_bytes"], half_encoder)
        self.assertGreater(explicit["phases"]["encode"]["vram_bytes"], legacy["phases"]["encode"]["vram_bytes"] + half_encoder)
        self.assertGreater(explicit["vram_bytes"], legacy["vram_bytes"])


class PresetAdviceTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        reference = load_tables(REFERENCE_DEVICE_CLASS)
        tables = patch.object(memory, "_tables_for", lambda _device: reference)
        tables.start()
        self.addCleanup(tables.stop)
        resolve = patch.object(svc.hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0")
        resolve.start()
        self.addCleanup(resolve.stop)

    def test_everything_fits_a_roomy_machine(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        advice = memory._preset_advice(normalized, 512, 512, {"vram_total": 64 * GIB, "vram_free": 64 * GIB, "ram_total": 128 * GIB, "ram_free": 128 * GIB})
        labels = [f"«{memory.VRAM_STEP_LABELS[pair.vram]} + {memory.RAM_STEP_LABELS[pair.ram]}»" for pair in STEP_WALK]
        self.assertEqual(advice, "Сейчас помещаются: " + ", ".join(labels) + ".")

    def test_nothing_fits_names_the_remaining_levers(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        advice = memory._preset_advice(normalized, 512, 512, {"vram_total": 1 << 20, "vram_free": 1 << 10, "ram_total": 1 << 20, "ram_free": 1 << 10})
        self.assertTrue(advice.startswith("Ни одно сочетание «Видеопамять» и «Оперативная память»"))


if __name__ == "__main__":
    unittest.main()
