"""
File: modules/ai_backend/runtime/large_models/test_cost_tables.py

Purpose:
Unit tests for `cost_tables.py`: the tracked fitted tables (`rocm_gfx1201`,
`cuda_sm_120`) parse and keep the properties of a fit (calibrated, measured
FLUX.3 and FLUX.2 entries, positive rates; the fitted numbers themselves are
never pinned), entry lookup by specificity, wildcard
entries never count as measured, an unknown device class borrows the reference
table conservatively and uncalibrated, schema refusals, and the device-class
key.
"""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

from . import cost_tables as ct


def _reference() -> dict:
    return json.loads((ct.CALIBRATION_DIR / f"{ct.REFERENCE_DEVICE_CLASS}.json").read_text(encoding="utf-8"))


class TrackedTableTests(unittest.TestCase):
    def test_reference_table_is_calibrated_and_has_flux3_entries(self) -> None:
        tables = ct.load_tables(ct.REFERENCE_DEVICE_CLASS)
        self.assertTrue(tables.calibrated)
        denoise = tables.phase_cost(family="flux3_klein", phase="denoise", variant="", format="nvfp4", compute_path="dequant", exec="device")
        self.assertTrue(denoise.measured)
        self.assertEqual(denoise.format, "nvfp4")
        self.assertGreater(denoise.bytes_per_token, 0)
        decode = tables.phase_cost(family="flux3_klein", phase="decode", variant="float32", format="bf16", compute_path="native", exec="device")
        self.assertTrue(decode.measured)
        self.assertGreater(decode.bytes_per_pixel, 0)

    def test_tracked_tables_are_fitted_tables(self) -> None:
        """Properties every fitted table keeps (the numbers themselves are the fit's, never pinned here)."""
        for device_class in (ct.REFERENCE_DEVICE_CLASS, "cuda_sm_120"):
            with self.subTest(device_class=device_class):
                tables = ct.load_tables(device_class)
                self.assertTrue(tables.calibrated)
                self.assertEqual(tables.device_class, device_class)
                measured_families = {entry.family for entry in tables.phases if entry.measured}
                self.assertLessEqual({"flux3_klein", "flux2_klein"}, measured_families)
                self.assertNotIn("*", measured_families, "a catch-all entry is never measured")
                for entry in tables.phases:
                    self.assertGreaterEqual(min(entry.fixed_bytes, entry.bytes_per_token, entry.bytes_per_pixel), 0, entry)
                    self.assertGreaterEqual(entry.reserved_factor, 1.0, entry)
                    if entry.measured and entry.exec == "device":
                        self.assertGreater(entry.fixed_bytes + entry.bytes_per_token + entry.bytes_per_pixel, 0, entry)
                device = tables.device
                for rate in (device.park_bytes_per_s, device.restore_bytes_per_s, device.block_h2d_bytes_per_s, device.disk_cold_bytes_per_s, device.disk_warm_bytes_per_s):
                    self.assertGreater(rate, 0)
                self.assertGreaterEqual(device.prefetch_depth, 1)

    def test_unknown_family_falls_back_to_unmeasured_wildcard(self) -> None:
        tables = ct.load_tables(ct.REFERENCE_DEVICE_CLASS)
        entry = tables.phase_cost(family="no_such_family", phase="denoise", variant="", format="bf16", compute_path="native", exec="device")
        self.assertEqual(entry.family, "*")
        self.assertFalse(entry.measured)
        cpu = tables.phase_cost(family="no_such_family", phase="encode", variant="", format="bf16", compute_path="native", exec="cpu")
        self.assertEqual(cpu.exec, "cpu")
        self.assertFalse(cpu.measured)

    def test_unknown_device_class_borrows_conservatively(self) -> None:
        own = ct.load_tables(ct.REFERENCE_DEVICE_CLASS)
        borrowed = ct.load_tables("cuda_sm_999")
        self.assertFalse(borrowed.calibrated)
        self.assertEqual(borrowed.device_class, "cuda_sm_999")
        self.assertTrue(all(not entry.measured for entry in borrowed.phases))
        self.assertGreater(borrowed.phases[0].bytes_per_token, own.phases[0].bytes_per_token)
        self.assertLess(borrowed.device.disk_cold_bytes_per_s, own.device.disk_cold_bytes_per_s)


class LookupTests(unittest.TestCase):
    def test_most_specific_entry_wins_first_on_tie(self) -> None:
        data = _reference()
        data["phases"].insert(0, {**data["phases"][0], "format": "*", "compute_path": "*", "fixed_bytes": 7})
        data["phases"].insert(0, {**data["phases"][1], "fixed_bytes": 9})
        tables = ct.parse_tables(data, source="test")
        exact = tables.phase_cost(family="flux3_klein", phase="denoise", variant="", format="bf16", compute_path="native", exec="device")
        self.assertEqual(exact.fixed_bytes, 9, "the first of two equally specific entries")
        other = tables.phase_cost(family="flux3_klein", phase="denoise", variant="", format="int4", compute_path="native", exec="device")
        self.assertEqual(other.fixed_bytes, 7)


class SchemaTests(unittest.TestCase):
    def _refused(self, data: object, pattern: str) -> None:
        with self.assertRaisesRegex(ValueError, pattern):
            ct.parse_tables(data, source="test")

    def test_refusals(self) -> None:
        good = _reference()
        self._refused([], "корень")
        bad = copy.deepcopy(good)
        bad["phases"] = [entry for entry in bad["phases"] if entry["family"] != "*" or entry["exec"] != "cpu"]
        self._refused(bad, "exec=cpu")
        bad = copy.deepcopy(good)
        bad["phases"][0]["reserved_factor"] = 0.5
        self._refused(bad, "reserved_factor")
        bad = copy.deepcopy(good)
        bad["phases"][0]["bytes_per_token"] = -1
        self._refused(bad, "bytes_per_token")
        bad = copy.deepcopy(good)
        bad["phases"][0]["exec"] = "gpu"
        self._refused(bad, "exec")
        bad = copy.deepcopy(good)
        bad["device"]["disk_cold_bytes_per_s"] = 0
        self._refused(bad, "disk_cold_bytes_per_s")
        bad = copy.deepcopy(good)
        del bad["calibrated"]
        self._refused(bad, "calibrated")

    def test_malformed_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / f"{ct.REFERENCE_DEVICE_CLASS}.json"
            path.write_text("{not json", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "не JSON"):
                ct.load_tables("anything", directory=Path(directory))
            path.unlink()
            with self.assertRaises(OSError):
                ct.load_tables("anything", directory=Path(directory))


class DeviceClassTests(unittest.TestCase):
    def test_keys(self) -> None:
        self.assertEqual(ct.device_class_of(name="AMD Radeon AI PRO R9700", arch="gfx1201:sramecc-:xnack-", is_hip=True, cuda_capability=None), "rocm_gfx1201")
        self.assertEqual(ct.device_class_of(name="RTX 5070", arch=None, is_hip=False, cuda_capability=(12, 0)), "cuda_sm_120")
        self.assertEqual(ct.device_class_of(name="?", arch=None, is_hip=True, cuda_capability=None), ct.GENERIC_DEVICE_CLASS)
        self.assertEqual(ct.device_class_of(name="?", arch=None, is_hip=False, cuda_capability=None), ct.GENERIC_DEVICE_CLASS)

    def test_detect_without_gpu_is_generic(self) -> None:
        self.assertEqual(ct.detect_device_class("cpu"), ct.GENERIC_DEVICE_CLASS)


if __name__ == "__main__":
    unittest.main()
