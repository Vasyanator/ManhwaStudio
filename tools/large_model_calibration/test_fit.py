"""
File: tools/large_model_calibration/test_fit.py

Purpose:
Unit tests of `fit.py`: the upper envelope and the non-negative least squares,
the entry key (format-free off the DiT), point collection from a synthetic
report (resident / juggled / CPU phases, overlapped and degraded steps left
out), the table build over a base table (measured flags, catch-alls, the
device constants, the manager's own schema check), and the acceptance check.

Run from the repository root:
`venv/bin/python -m pytest tools/large_model_calibration -q -p no:cacheprovider`
(or `-m unittest discover -s tools/large_model_calibration -t .`). No GPU,
no torch, no files outside a temporary directory.
"""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any

from modules.ai_backend.runtime.large_models.cost_tables import CALIBRATION_DIR, REFERENCE_DEVICE_CLASS, parse_tables

from . import fit

GIB = 1024**3
MIB = 1024**2


def _placement(tier: str, exec_device: str = "run_device", device_bytes: int = 0, host_bytes: int = 0) -> dict[str, Any]:
    return {"tier": tier, "exec_device": exec_device, "blocks_device": 0, "blocks_host": 0, "blocks_disk": 0, "device_bytes": device_bytes, "host_bytes": host_bytes}


def _handle(**components: dict[str, Any]) -> dict[str, Any]:
    return {"key": {"model": "toy", "components": components}}


def _work(phase: str, compute: str, role: str, *, tokens: int = 0, pixels: int = 0, passes: int = 1, variant: str = "", weight_format: str = "bf16", compute_path: str = "native") -> dict[str, Any]:
    return {"family": "toy", "phase": phase, "variant": variant, "format": weight_format, "compute_path": compute_path, "compute": compute, "role": role, "tokens": tokens, "pixels": pixels, "passes": passes, "extra_device_bytes": 0}


def _step(phase: str, *, work: dict[str, Any] | None, placements: dict[str, Any], peak_alloc: int = 0, peak_reserved: int = 0, wall: float = 1.0, begin: dict[str, Any] | None = None, rss_begin: int = 0, rss_peak: int = 0, overlapped: bool = False, non_torch: int | None = 300 * MIB, action: str = "") -> dict[str, Any]:
    return {"phase": phase, "info": {"action": action}, "work": work, "ended": True, "failed": False, "overlapped": overlapped, "placements_begin": begin if begin is not None else placements, "placements_end": placements, "peak_alloc": peak_alloc, "peak_reserved": peak_reserved, "wall_s": wall, "rss_begin": rss_begin, "rss_peak": rss_peak, "non_torch_end": non_torch}


def _denoise(tokens: int, alloc: int, reserved: int, wall: float, *, weights: int = 10 * GIB, tier: str = "device", host: int = 0, overlapped: bool = False) -> dict[str, Any]:
    placements = _handle(dit=_placement(tier, device_bytes=weights, host_bytes=host))
    return _step("denoise", work=_work("denoise", "dit", "dit", tokens=tokens, passes=4), placements=placements, peak_alloc=weights + alloc, peak_reserved=weights + reserved, wall=wall, overlapped=overlapped)


def _report(*combos: dict[str, Any], device_class: str = "test_class") -> dict[str, Any]:
    return {"environment": {"device_class": device_class, "device_name": "toy", "torch": "x"}, "combos": list(combos), "grids": []}


def _combo(combo_id: str, *steps: dict[str, Any], status: str = "ok", degraded: bool = False, planned: dict[str, Any] | None = None, measured: tuple[float, float] = (1.0, 0.5)) -> dict[str, Any]:
    return {"combo_id": combo_id, "status": status, "degraded": degraded, "phases": list(steps), "planned": planned or {"fits": True}, "vram_step": "max", "ram_step": "keep", "vram_effective": "max", "ram_effective": "keep", "forecast_device_peak_gib": 2.0, "measured_device_peak_gib": measured[0], "forecast_host_growth_gib": 1.0, "measured_host_growth_gib": measured[1]}


def _base() -> dict[str, Any]:
    return json.loads((CALIBRATION_DIR / f"{REFERENCE_DEVICE_CLASS}.json").read_text(encoding="utf-8"))


class EnvelopeTests(unittest.TestCase):
    def test_covers_every_point_with_the_least_excess(self) -> None:
        xs, ys = [1.0, 2.0, 3.0, 4.0], [1.0, 3.0, 4.0, 4.5]
        a, b = fit.upper_envelope(xs, ys)
        self.assertGreaterEqual(min(a, b), 0.0)
        for x, y in zip(xs, ys):
            self.assertGreaterEqual(a + b * x, y - 1e-9)
        # The hull edge through (2, 3) and (3, 4) is the optimum here.
        self.assertAlmostEqual(b, 1.0)
        self.assertAlmostEqual(a, 1.0)

    def test_falling_points_give_a_flat_line_and_one_point_works(self) -> None:
        self.assertEqual(fit.upper_envelope([1.0, 2.0], [5.0, 3.0]), (5.0, 0.0))
        a, b = fit.upper_envelope([2.0], [4.0])
        self.assertGreaterEqual(a + 2.0 * b, 4.0)

    def test_no_points_is_an_error(self) -> None:
        with self.assertRaises(ValueError):
            fit.upper_envelope([], [])


class NnlsTests(unittest.TestCase):
    def test_exact_and_clamped(self) -> None:
        solution = fit.nnls([[1.0, 1.0], [1.0, 2.0], [1.0, 3.0]], [2.0, 4.0, 6.0])
        self.assertAlmostEqual(solution[0], 0.0, places=6)
        self.assertAlmostEqual(solution[1], 2.0, places=6)
        # Unconstrained LS would need a negative intercept; it is clamped to 0.
        clamped = fit.nnls([[1.0, 1.0], [1.0, 2.0]], [1.0, 5.0])
        self.assertTrue(all(value >= 0 for value in clamped))


class KeyTests(unittest.TestCase):
    def test_format_only_on_the_dit(self) -> None:
        self.assertEqual(fit.entry_key(_work("denoise", "dit", "dit", weight_format="nvfp4", compute_path="dequant"), "device"), ("toy", "denoise", "", "nvfp4", "dequant", "device"))
        self.assertEqual(fit.entry_key(_work("decode", "ae", "ae", variant="float32", weight_format="nvfp4", compute_path="dequant"), "device"), ("toy", "decode", "float32", "*", "*", "device"))


class CollectAndBuildTests(unittest.TestCase):
    def setUp(self) -> None:
        self.base = _base()
        # Resident denoise at two sizes (twice at the larger one: the faster time wins),
        # a host-juggled denoise, an overlapped step, a CPU encode, a park, a degraded combo.
        juggled = _denoise(2000, 300 * MIB, 400 * MIB, 14.0, weights=1 * GIB, tier="host", host=8 * GIB)
        encoder_host = _handle(encoder=_placement("host", "cpu", host_bytes=6 * GIB))
        encode = _step("encode", work=_work("encode", "encoder", "encoder", tokens=512, variant="text"), placements=encoder_host, rss_begin=8 * GIB, rss_peak=8 * GIB + 200 * MIB, wall=3.0)
        park = _step("denoise->decode", work=None, begin=_handle(dit=_placement("device", device_bytes=10 * GIB)), placements=_handle(dit=_placement("host", host_bytes=10 * GIB)), wall=2.0)
        self.reports = [
            (Path("/r/a/report.json"), _report(
                _combo("a", _denoise(1000, 100 * MIB, 180 * MIB, 4.4), _denoise(2000, 300 * MIB, 420 * MIB, 12.0), encode, park),
                _combo("b", _denoise(2000, 280 * MIB, 380 * MIB, 10.0), juggled, _denoise(3000, 9 * GIB, 9 * GIB, 1.0, overlapped=True)),
                _combo("c", _denoise(4000, 50 * GIB, 50 * GIB, 1.0), degraded=True),
            )),
        ]

    def test_points_skip_overlapped_and_degraded_and_split_exec(self) -> None:
        points = fit.collect_points(self.reports, prefetch_depth=1)
        denoise = points[("toy", "denoise", "", "bf16", "native", "device")]
        self.assertEqual(len(denoise), 4, "two sizes, a repeat and the juggled pass; overlapped / degraded steps are left out")
        self.assertEqual(sum(1 for point in denoise if not point.resident), 1)
        cpu = points[("toy", "encode", "text", "*", "*", "cpu")]
        self.assertEqual(cpu[0].alloc, 200 * MIB)
        self.assertFalse(next(point for point in denoise if not point.resident).sized, "a juggled phase without block sizes has no usable bytes")

    def test_a_dropped_handle_of_another_format_is_not_read(self) -> None:
        placements = {"a_stale": {"model": "toy", "components": {"dit": _placement("device")}}, "b_live": {"model": "toy", "components": {"dit": _placement("device", device_bytes=10 * GIB)}}}
        placements["a_stale"]["components"]["dit"]["tier"] = None
        step = _step("denoise", work=_work("denoise", "dit", "dit", tokens=1000, passes=4), placements=placements, peak_alloc=10 * GIB + 100 * MIB, peak_reserved=10 * GIB + 120 * MIB)
        point = fit.collect_points([(Path("/r/s/report.json"), _report(_combo("s", step)))], prefetch_depth=1)[("toy", "denoise", "", "bf16", "native", "device")][0]
        self.assertTrue(point.resident)
        self.assertEqual(point.alloc, 100 * MIB)

    def test_juggled_staging_is_taken_out(self) -> None:
        work = {**_work("denoise", "dit", "dit", tokens=1000, passes=4), "block_bytes": [1 * GIB, 2 * GIB, 1 * GIB]}
        placement = {**_placement("host", device_bytes=3 * GIB, host_bytes=3 * GIB), "blocks_device": 1}
        step = _step("denoise", work=work, placements={"key": {"model": "toy", "components": {"dit": placement}}}, peak_alloc=3 * GIB + 4 * GIB + 100 * MIB, peak_reserved=3 * GIB + 4 * GIB + 150 * MIB)
        points = fit.collect_points([(Path("/r/j/report.json"), _report(_combo("j", step)))], prefetch_depth=1)
        point = points[("toy", "denoise", "", "bf16", "native", "device")][0]
        self.assertTrue(point.sized)
        self.assertFalse(point.resident)
        self.assertEqual(point.alloc, 100 * MIB, "two slots of the largest non-resident block (2 GiB) are staging, not activation")
        self.assertEqual(point.host_remainder, 3 * GIB)

    def test_table_covers_every_point_and_keeps_the_base(self) -> None:
        table, evidence = fit.build_table(self.reports, self.base, "test_class", "unit test")
        parse_tables(table, source="unit test")
        entries = {fit._key_of(entry): entry for entry in table["phases"]}
        denoise = entries[("toy", "denoise", "", "bf16", "native", "device")]
        self.assertTrue(denoise["measured"])
        for x, alloc, reserved in ((1000, 100 * MIB, 180 * MIB), (2000, 300 * MIB, 420 * MIB)):
            envelope = denoise["fixed_bytes"] + denoise["bytes_per_token"] * x
            self.assertGreaterEqual(envelope, alloc - 1)
            self.assertGreaterEqual(envelope * denoise["reserved_factor"], reserved - 1)
        self.assertEqual(denoise["seconds_fixed"], 0.0)
        self.assertGreater(denoise["seconds_per_ktoken_pass"] + denoise["seconds_per_ktoken2_pass"], 0)
        cpu = entries[("toy", "encode", "text", "*", "*", "cpu")]
        self.assertTrue(cpu["measured"])
        self.assertEqual(cpu["reserved_factor"], 1.0)
        # A base entry the runs did not reach: kept, but not "measured" on another class.
        kept = entries[("flux3_klein", "decode", "float32", "*", "*", "device")]
        self.assertFalse(kept["measured"])
        catch_alls = [entry for entry in table["phases"] if entry["family"] == "*"]
        self.assertEqual(len(catch_alls), 2)
        self.assertTrue(all(not entry["measured"] for entry in catch_alls))
        # Device constants: the park rate (10 GiB / 2 s) and the largest non-torch memory.
        self.assertAlmostEqual(table["device"]["park_bytes_per_s"], 5 * GIB)
        self.assertEqual(table["device"]["runtime_overhead_bytes"], 300 * MIB)
        self.assertIn("block_h2d", evidence["device"])
        self.assertEqual(table["device"]["disk_cold_bytes_per_s"], self.base["device"]["disk_cold_bytes_per_s"])

    def test_same_class_base_keeps_its_measured_flags(self) -> None:
        table, _evidence = fit.build_table(self.reports, self.base, REFERENCE_DEVICE_CLASS, "unit test")
        entries = {fit._key_of(entry): entry for entry in table["phases"]}
        self.assertTrue(entries[("flux3_klein", "decode", "float32", "*", "*", "device")]["measured"])

    def test_single_size_scales_the_base_shape(self) -> None:
        base = copy.deepcopy(self.base)
        toy = {"family": "toy", "phase": "denoise", "variant": "", "format": "bf16", "compute_path": "native", "exec": "device", "measured": True, "fixed_bytes": 10 * MIB, "bytes_per_token": 1000.0, "bytes_per_pixel": 0.0, "reserved_factor": 1.5, "seconds_fixed": 0.0, "seconds_per_ktoken_pass": 0.1, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}
        base["phases"].insert(0, toy)
        reports = [(Path("/r/s/report.json"), _report(_combo("s", _denoise(1000, 100 * MIB, 150 * MIB, 4.0))))]
        table, _evidence = fit.build_table(reports, base, "test_class", "unit test")
        entry = next(entry for entry in table["phases"] if entry["family"] == "toy")
        self.assertGreaterEqual(entry["fixed_bytes"] + entry["bytes_per_token"] * 1000, 100 * MIB - 1)
        self.assertAlmostEqual(entry["fixed_bytes"] / entry["bytes_per_token"], 10 * MIB / 1000.0, delta=1.0)


class CheckTests(unittest.TestCase):
    def test_misses_and_over_forecast(self) -> None:
        reports = [(Path("/r/c/report.json"), _report(_combo("ok", measured=(1.5, 0.5)), _combo("miss", measured=(2.5, 1.5)), _combo("deg", degraded=True, measured=(9.0, 9.0))))]
        result = fit.check_reports(reports)
        self.assertEqual(result["judged"], 2)
        self.assertEqual([row["combo_id"] for row in result["device_misses"]], ["miss"])
        self.assertEqual([row["combo_id"] for row in result["host_misses"]], ["miss"])
        self.assertAlmostEqual(result["mean_device_over_forecast_gib"], 0.0)

    def test_first_combo_of_a_process_is_judged_apart_on_the_host(self) -> None:
        refused = {**_combo("refused", measured=(1.0, 9.0)), "status": "error"}
        first = {**_combo("first", _denoise(1000, 1, 1, 1.0), measured=(1.0, 1.4))}
        later = {**_combo("later", _denoise(1000, 1, 1, 1.0), measured=(1.0, 1.4))}
        result = fit.check_reports([(Path("/r/f/report.json"), _report(refused, first, later))])
        self.assertEqual([row["combo_id"] for row in result["host_misses"]], ["later"])
        self.assertEqual([row["combo_id"] for row in result["first_in_process_host_misses"]], ["first"])


class LoadTests(unittest.TestCase):
    def test_directories_are_searched_and_missing_paths_refused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            nested = Path(directory) / "run" / "report.json"
            nested.parent.mkdir()
            nested.write_text(json.dumps(_report()), encoding="utf-8")
            self.assertEqual(len(fit.load_reports([Path(directory)])), 1)
            with self.assertRaises(FileNotFoundError):
                fit.load_reports([Path(directory) / "absent"])


if __name__ == "__main__":
    unittest.main()
