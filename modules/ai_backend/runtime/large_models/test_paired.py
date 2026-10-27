"""
File: modules/ai_backend/runtime/large_models/test_paired.py

Purpose:
Contracts of `paired`, the two-handle memory rule (pipeline + text encoder),
on synthetic forecasts and a scripted manager: the combined device peak (the
encode moment of an encoder off the device included) and
the encoder's cap for both `PairPhases` in use (FLUX.2: the encoder beside the
denoise state; FLUX.3: beside the reference encode), the ceiling chain, the
reservation / baseline arithmetic, `EncoderNeed.counted`, the three-step
`plan_pair` call sequence, the wire answer (`PairedReport.to_wire`, the
refusal merge included), the cap of an admitted session and the run peaks.

Notes:
- Pure: no files, no torch. The FLUX.2 composition over a REAL manager is
  pinned by `inpaint/flux2_klein/test_characterization.ProfilesGoldenTests`.
"""

from __future__ import annotations

import copy
import types
import unittest
from dataclasses import dataclass, field
from typing import Any

from . import paired
from .forecast import Forecast, PhaseForecast, TimeForecast
from .paired import (
    EncoderNeed,
    HandleRequest,
    PairedReport,
    PairPhases,
    combined_ceilings,
    combined_device_peak,
    encoder_cap_beside,
    encoder_device_cap,
    host_baseline,
    plan_pair,
    reservation_of,
    with_host_baseline,
)
from .planner import FitKind, PlanFit, Planned, PlanRefusal
from .tiers import IDLE_PHASE, RamStep, Steps, VramStep

FLUX2 = PairPhases(pipeline=("denoise", "decode"), rest="denoise", encode="encode")
FLUX3 = PairPhases(pipeline=("ref_encode", "denoise", "decode"), rest="ref_encode", encode="encode")

BASELINE = 1000


def _phase(device_peak: int = 0, device_weights: int = 0, host_peak: int = BASELINE, host_weights: int = 0) -> PhaseForecast:
    return PhaseForecast(device_peak=device_peak, host_peak=host_peak, seconds=0.0, device_weights=device_weights, device_activation=device_peak - device_weights, host_weights=host_weights, host_activation=0)


def _forecast(phases: dict[str, PhaseForecast], *, idle_host_weights: int = 0) -> Forecast:
    """A forecast of `phases` plus an idle state whose host peak is the baseline plus `idle_host_weights`."""
    every = {**phases, IDLE_PHASE: _phase(host_peak=BASELINE + idle_host_weights, host_weights=idle_host_weights)}
    return Forecast(phases=every, transitions={}, device_peak=max(entry.device_peak for entry in every.values()), host_peak=max(entry.host_peak for entry in every.values()), peak_phase="", time=TimeForecast(0.0, 0.0, 0.0, 0.0), plan_summary=(), calibrated=True, device_calibrated=True)


def _pipeline(*, denoise: tuple[int, int] = (100, 60), decode: tuple[int, int] = (80, 20), ref_encode: tuple[int, int] | None = None, host_peak: int = BASELINE + 50) -> Forecast:
    """A pipeline forecast: each phase `(device peak, device weights)`; `ref_encode` only for the FLUX.3 shape."""
    phases = {"denoise": _phase(*denoise, host_peak=host_peak), "decode": _phase(*decode)}
    if ref_encode is not None:
        phases["ref_encode"] = _phase(*ref_encode)
    return _forecast(phases)


def _encoder(peak: int = 30, *, host_peak: int = BASELINE + 70) -> Forecast:
    return _forecast({"encode": _phase(peak, peak, host_peak=host_peak)})


def _planned(forecast: Forecast, steps: Steps, *, full: Forecast | None = None) -> Planned:
    # `plan_pair` / `live_ceilings` read only `plan.steps` of a plan.
    return Planned(plan=types.SimpleNamespace(steps=steps), forecast=forecast, fit=PlanFit(FitKind.FREE), full_forecast=full or forecast)  # type: ignore[arg-type]


def _refusal(forecast: Forecast | None = None, *, reason: str = "host_short", resource: str = "host") -> PlanRefusal:
    return PlanRefusal(reason=reason, shortest_resource=resource, best_steps_tried=None, forecast=forecast)


@dataclass
class _Report:
    """A `ProfilesReport` stand-in: `selected`, the VRAM column and a fixed wire dict."""

    selected: Any
    name: str
    vram_rows: dict[VramStep, Any] = field(default_factory=dict)
    vram_applicable: bool = True

    def to_dict(self) -> dict[str, Any]:
        shown = self.selected.full_forecast if isinstance(self.selected, Planned) else self.selected.forecast
        fits = isinstance(self.selected, Planned)
        steps = self.selected.plan.steps if fits else None
        return copy.deepcopy({
            "model": self.name,
            "selected": {
                "fits": fits,
                "vram": steps.vram.value if steps else None,
                "ram": steps.ram.value if steps else None,
                "needs_reclaim": None,
                "reason": None if fits else self.selected.reason,
                "shortest_resource": None if fits else self.selected.shortest_resource,
                "forecast": {"device_peak": shown.device_peak, "host_peak": shown.host_peak} if shown is not None else None,
            },
            "formats": [{"id": f"{self.name}-format"}],
        })


class _Manager:
    """A scripted manager: `profiles` records its call and answers `answer(model, kwargs)`."""

    def __init__(self, answer: Any) -> None:
        self.answer = answer
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def profiles(self, model: str, options: Any, shape: Any, **kwargs: Any) -> Any:
        self.calls.append((model, kwargs))
        return self.answer(model, kwargs)


PIPELINE = HandleRequest("pipe", "pipe-options", "bf16")
ENCODER = HandleRequest("enc", "enc-options", "q8")


class CombinedPeakTests(unittest.TestCase):
    def test_flux2_the_encode_moment_sits_beside_the_denoise_weights(self) -> None:
        pipeline = _pipeline(denoise=(100, 60), decode=(80, 20))
        self.assertEqual(combined_device_peak(pipeline, None, FLUX2), 100)
        self.assertEqual(combined_device_peak(pipeline, _encoder(30), FLUX2), 100, "60 + 30 stays under the denoise peak")
        self.assertEqual(combined_device_peak(pipeline, _encoder(70), FLUX2), 130)

    def test_flux3_the_encode_moment_sits_beside_the_reference_encode_weights(self) -> None:
        pipeline = _pipeline(denoise=(100, 60), decode=(80, 20), ref_encode=(90, 10))
        self.assertEqual(combined_device_peak(pipeline, None, FLUX3), 100)
        self.assertEqual(combined_device_peak(pipeline, _encoder(95), FLUX3), 105, "10 (ref_encode weights) + 95")
        self.assertEqual(combined_device_peak(pipeline, _encoder(95), FLUX2), 155, "the FLUX.2 binding reads the denoise weights")

    def test_a_pipeline_phase_bounds_the_peak(self) -> None:
        pipeline = _pipeline(denoise=(10, 5), decode=(10, 5), ref_encode=(200, 5))
        self.assertEqual(combined_device_peak(pipeline, None, FLUX3), 200)
        self.assertEqual(combined_device_peak(pipeline, None, FLUX2), 10, "FLUX.2 has no ref_encode phase")

    def test_an_encoder_forecast_without_its_phase_encodes_nothing(self) -> None:
        self.assertEqual(combined_device_peak(_pipeline(), _forecast({"other": _phase(500, 500)}), FLUX2), 100)

    def test_an_encoder_off_the_device_adds_only_the_pipelines_context(self) -> None:
        """A CPU-pinned encoder forecasts device peak 0: the encode moment is the resting weights plus the pipeline's `device_overhead`, counted once."""
        from dataclasses import replace

        pipeline = replace(_pipeline(denoise=(100, 60), decode=(80, 20)), device_overhead=7)
        on_cpu = _encoder(0)
        self.assertEqual(paired.encode_moment_device(pipeline, on_cpu, FLUX2), 67)
        self.assertEqual(paired.encode_moment_device(pipeline, _encoder(30), FLUX2), 90, "an encoder on the device carries its own overhead")
        self.assertEqual(paired.encode_moment_device(pipeline, None, FLUX2), 0)
        self.assertEqual(combined_device_peak(pipeline, on_cpu, FLUX2), 100, "the pipeline's own phases still bound the run")


class EncoderCapTests(unittest.TestCase):
    def test_no_ceiling_no_cap(self) -> None:
        self.assertIsNone(encoder_device_cap(None, _pipeline(), FLUX2))

    def test_the_cap_is_the_ceiling_minus_the_rest_state_weights(self) -> None:
        pipeline = _pipeline(denoise=(100, 60), ref_encode=(90, 10))
        self.assertEqual(encoder_device_cap(150, pipeline, FLUX2), 90)
        self.assertEqual(encoder_device_cap(150, pipeline, FLUX3), 140)

    def test_the_cap_never_goes_negative(self) -> None:
        self.assertEqual(encoder_device_cap(10, _pipeline(denoise=(100, 60)), FLUX2), 0)


class CeilingChainTests(unittest.TestCase):
    def test_each_step_is_capped_by_the_nearest_fitting_higher_step(self) -> None:
        peaks = {VramStep.MAX: 500, VramStep.PHASE: None, VramStep.HALF: 300}
        seen: list[tuple[Steps, int | None]] = []

        def combined_at(cell: Steps, ceiling: int | None) -> int | None:
            seen.append((cell, ceiling))
            return peaks[cell.vram]

        ceilings = combined_ceilings(RamStep.RUN, combined_at, until=VramStep.MIN)
        self.assertEqual(ceilings, {VramStep.MAX: None, VramStep.PHASE: 500, VramStep.HALF: 500, VramStep.MIN: 300})
        self.assertEqual(seen, [(Steps(VramStep.MAX, RamStep.RUN), None), (Steps(VramStep.PHASE, RamStep.RUN), 500), (Steps(VramStep.HALF, RamStep.RUN), 500)])

    def test_the_until_step_is_never_planned(self) -> None:
        self.assertEqual(combined_ceilings(RamStep.KEEP, lambda _cell, _ceiling: self.fail("planned"), until=VramStep.MAX), {VramStep.MAX: None})


class ReservationTests(unittest.TestCase):
    def test_the_baseline_is_read_from_the_idle_state(self) -> None:
        forecast = _forecast({"encode": _phase(10, 10, host_peak=BASELINE + 70)}, idle_host_weights=40)
        self.assertEqual(host_baseline(forecast), BASELINE)
        self.assertEqual(reservation_of(forecast), (10, 70))

    def test_with_host_baseline_reads_the_shown_forecast(self) -> None:
        planned = _planned(_encoder(), Steps(VramStep.MAX, RamStep.KEEP))
        selected = {"fits": True, "forecast": {"host_peak": 1}}
        self.assertEqual(with_host_baseline(selected, planned)["forecast"], {"host_peak": 1, "host_baseline": BASELINE})
        self.assertEqual(with_host_baseline(selected, _refusal(_encoder()))["forecast"]["host_baseline"], BASELINE)
        self.assertIs(with_host_baseline(selected, _refusal(None)), selected, "a refusal without a forecast")
        bare = {"fits": False, "forecast": None}
        self.assertIs(with_host_baseline(bare, planned), bare, "a selected without a forecast")


class EncoderNeedTests(unittest.TestCase):
    def test_left_out_only_when_neither_encoding_nor_resident(self) -> None:
        for encodes in (False, True):
            for resident in (False, True):
                with self.subTest(encodes=encodes, resident=resident):
                    self.assertEqual(EncoderNeed(encodes=encodes, resident=resident).counted, encodes or resident)


class PlanPairTests(unittest.TestCase):
    def _answer(self, selected_steps: Steps, *, encoder_fits: bool = True) -> Any:
        rows = {vram: _planned(_pipeline(denoise=(100 + index, 60)), Steps(vram, selected_steps.ram)) for index, vram in enumerate((VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN))}

        def answer(model: str, kwargs: dict[str, Any]) -> Any:
            if model == "pipe":
                return _Report(rows[selected_steps.vram], "pipe", vram_rows=rows)
            if encoder_fits:
                return _Report(_planned(_encoder(30), kwargs["steps"] if kwargs["steps"].vram is not VramStep.AUTO else Steps(VramStep.MAX, RamStep.KEEP)), "enc")
            return _Report(_refusal(_encoder(30)), "enc")

        return answer

    def test_an_uncounted_encoder_plans_the_pipeline_alone(self) -> None:
        manager = _Manager(self._answer(Steps(VramStep.MAX, RamStep.KEEP)))
        report = plan_pair(manager, PIPELINE, None, "shape", "cuda:0", Steps(VramStep.MAX, RamStep.KEEP), FLUX2)  # type: ignore[arg-type]
        self.assertEqual([model for model, _kwargs in manager.calls], ["pipe"])
        self.assertEqual(manager.calls[0][1], {"device": "cuda:0", "format": "bf16", "steps": Steps(VramStep.MAX, RamStep.KEEP)})
        wire = report.to_wire()
        self.assertIsNone(wire["encoder"])
        self.assertEqual(wire["encoder_formats"], [])
        self.assertEqual(wire["selected"]["forecast"]["host_baseline"], BASELINE)

    def test_the_top_step_admits_the_encoder_uncapped_then_replans_the_pipeline(self) -> None:
        steps = Steps(VramStep.MAX, RamStep.KEEP)
        manager = _Manager(self._answer(steps))
        report = plan_pair(manager, PIPELINE, ENCODER, "shape", "cuda:0", steps, FLUX2)  # type: ignore[arg-type]
        self.assertEqual([model for model, _kwargs in manager.calls], ["pipe", "enc", "pipe"])
        _alone, encoder, combined = (kwargs for _model, kwargs in manager.calls)
        self.assertEqual((encoder["format"], encoder["claimed_device_bytes"], encoder["claimed_host_bytes"], encoder["device_cap_bytes"]), ("q8", 100, 50, None))
        self.assertEqual((combined["claimed_device_bytes"], combined["claimed_host_bytes"]), (30, 70))
        wire = report.to_wire()
        self.assertEqual(wire["encoder_formats"], [{"id": "enc-format"}])
        self.assertTrue(wire["selected"]["fits"])
        self.assertEqual(wire["encoder"]["forecast"]["host_baseline"], BASELINE)

    def test_a_lower_step_caps_the_encoder_by_the_higher_steps_combined_peak(self) -> None:
        steps = Steps(VramStep.PHASE, RamStep.KEEP)
        for phases, rest_weights in ((FLUX2, 60), (FLUX3, 10)):
            with self.subTest(phases=phases.rest):
                rows_ref = {vram: _planned(_pipeline(denoise=(100 + index, 60), ref_encode=(90, 10)), Steps(vram, RamStep.KEEP)) for index, vram in enumerate((VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN))}

                def answer(model: str, kwargs: dict[str, Any], _rows: dict[VramStep, Planned] = rows_ref) -> Any:
                    if model == "pipe":
                        return _Report(_rows[VramStep.PHASE], "pipe", vram_rows=_rows)
                    return _Report(_planned(_encoder(30), kwargs["steps"]), "enc")

                manager = _Manager(answer)
                plan_pair(manager, PIPELINE, ENCODER, "shape", "cuda:0", steps, phases)  # type: ignore[arg-type]
                encoder_calls = [kwargs for model, kwargs in manager.calls if model == "enc"]
                self.assertEqual([kwargs["steps"] for kwargs in encoder_calls], [Steps(VramStep.MAX, RamStep.KEEP), steps])
                self.assertIsNone(encoder_calls[0]["device_cap_bytes"])
                # MAX's combined peak: max(100, 80, [90 for FLUX.3], rest weights + 30); the PHASE cap subtracts PHASE's rest weights.
                max_peak = max(100, 80, rest_weights + 30, *((90,) if phases is FLUX3 else ()))
                self.assertEqual(encoder_calls[1]["device_cap_bytes"], max_peak - rest_weights)

    def test_a_refused_encoder_merges_its_refusal_into_the_selection(self) -> None:
        steps = Steps(VramStep.MAX, RamStep.KEEP)
        manager = _Manager(self._answer(steps, encoder_fits=False))
        report = plan_pair(manager, PIPELINE, ENCODER, "shape", "cuda:0", steps, FLUX2)  # type: ignore[arg-type]
        self.assertEqual([model for model, _kwargs in manager.calls], ["pipe", "enc"], "no combined replan")
        self.assertIsNone(report.combined)
        wire = report.to_wire()
        self.assertEqual(wire["model"], "pipe")
        self.assertEqual({key: wire["selected"][key] for key in ("fits", "vram", "ram", "needs_reclaim", "reason", "shortest_resource")}, {"fits": False, "vram": None, "ram": None, "needs_reclaim": None, "reason": "host_short", "shortest_resource": "host"})
        self.assertEqual(wire["selected"]["forecast"]["host_baseline"], BASELINE)
        self.assertFalse(wire["encoder"]["fits"])
        self.assertEqual(wire["encoder_formats"], [{"id": "enc-format"}])

    def test_a_refused_pipeline_gives_the_encoder_no_claim_and_no_cap(self) -> None:
        def answer(model: str, kwargs: dict[str, Any]) -> Any:
            if model == "pipe":
                return _Report(_refusal(_pipeline(), reason="device_short", resource="device"), "pipe")
            return _Report(_planned(_encoder(30), Steps(VramStep.MAX, RamStep.KEEP)), "enc")

        manager = _Manager(answer)
        plan_pair(manager, PIPELINE, ENCODER, "shape", "cuda:0", Steps(VramStep.PHASE, RamStep.KEEP), FLUX2)  # type: ignore[arg-type]
        encoder = manager.calls[1][1]
        self.assertEqual((encoder["claimed_device_bytes"], encoder["claimed_host_bytes"], encoder["device_cap_bytes"]), (0, 0, None))

    def test_a_host_run_device_has_no_ceiling(self) -> None:
        steps = Steps(VramStep.PHASE, RamStep.KEEP)
        rows = {VramStep.PHASE: _planned(_pipeline(), steps)}

        def answer(model: str, kwargs: dict[str, Any]) -> Any:
            if model == "pipe":
                return _Report(rows[VramStep.PHASE], "pipe", vram_rows={}, vram_applicable=False)
            return _Report(_planned(_encoder(30), steps), "enc")

        manager = _Manager(answer)
        plan_pair(manager, PIPELINE, ENCODER, "shape", "cpu", steps, FLUX2)  # type: ignore[arg-type]
        self.assertEqual([model for model, _kwargs in manager.calls], ["pipe", "enc", "pipe"])
        self.assertIsNone(manager.calls[1][1]["device_cap_bytes"])


class RunPeakTests(unittest.TestCase):
    def test_the_run_peaks_are_the_admitted_plans(self) -> None:
        steps = Steps(VramStep.MAX, RamStep.KEEP)
        alone = _Report(_planned(_pipeline(denoise=(100, 60), ref_encode=(90, 10)), steps), "pipe")
        encoder = _Report(_planned(_encoder(95), steps), "enc")
        report = PairedReport(alone, encoder, alone)  # type: ignore[arg-type]
        self.assertEqual(report.run_device_peak(FLUX3), 105)
        self.assertEqual(report.run_device_peak(FLUX2), 155)
        self.assertEqual(report.run_host_peak(), BASELINE + 50 + 70, "the baseline counted once")
        self.assertEqual(PairedReport(alone, None, None).run_host_peak(), BASELINE + 50)  # type: ignore[arg-type]
        self.assertEqual(PairedReport(alone, None, None).run_device_peak(FLUX3), 100)  # type: ignore[arg-type]

    def test_a_refused_handle_has_no_run_peak(self) -> None:
        steps = Steps(VramStep.MAX, RamStep.KEEP)
        alone = _Report(_planned(_pipeline(), steps), "pipe")
        refused = _Report(_refusal(_encoder()), "enc")
        for report in (PairedReport(alone, refused, None), PairedReport(refused, None, None)):  # type: ignore[arg-type]
            self.assertIsNone(report.run_device_peak(FLUX2))
            self.assertIsNone(report.run_host_peak())


class EncoderCapBesideTests(unittest.TestCase):
    def _session(self, steps: Steps) -> Any:
        return types.SimpleNamespace(steps_effective=steps, forecast=_pipeline(denoise=(100, 60), ref_encode=(90, 10)))

    def test_the_cap_of_the_admitted_step(self) -> None:
        ceilings = (RamStep.KEEP, {VramStep.MAX: None, VramStep.PHASE: 150})
        self.assertEqual(encoder_cap_beside(ceilings, self._session(Steps(VramStep.PHASE, RamStep.KEEP)), FLUX2, owner="X"), 90)
        self.assertEqual(encoder_cap_beside(ceilings, self._session(Steps(VramStep.PHASE, RamStep.KEEP)), FLUX3, owner="X"), 140)
        self.assertIsNone(encoder_cap_beside(ceilings, self._session(Steps(VramStep.MAX, RamStep.KEEP)), FLUX2, owner="X"))
        self.assertIsNone(encoder_cap_beside(None, self._session(Steps(VramStep.PHASE, RamStep.KEEP)), FLUX2, owner="X"))

    def test_another_ram_step_gets_no_ceiling_and_says_so(self) -> None:
        ceilings = (RamStep.KEEP, {VramStep.MAX: None, VramStep.PHASE: 150})
        with self.assertLogs(paired.log, level="INFO") as logs:
            self.assertIsNone(encoder_cap_beside(ceilings, self._session(Steps(VramStep.PHASE, RamStep.RUN)), FLUX2, owner="FLUX.2 klein"))
        self.assertIn("FLUX.2 klein: пайплайн принят на шаге ОЗУ «run»", logs.output[0])


if __name__ == "__main__":
    unittest.main()
