"""
File: modules/ai_backend/runtime/test_device_service_large_limits.py

Purpose:
Unit tests for the large-model limits persistence of `device_service.py`
(`UserConfigLimitsStore` via `AiDeviceService.large_models_limits_store`) and
`AiDeviceService.general_section`, over an in-memory settings document
(`_MemoryUserConfig`): no real `user_config` is read or written.

Coverage:
- absent keys read as the defaults (no cap);
- a write stores exactly the two `General.large_models_*_cap_bytes` keys, a
  `None` removes its key, every other key survives;
- invalid stored values read as their default;
- a retired `General.large_models_keep_policy` value is ignored with one INFO
  log per store, and no write removes or rewrites it;
- floors go to the delegated floors store, never into the settings document;
- a `LargeModelManager` built over the store reads the persisted limits back;
- a failing settings write propagates and leaves the manager's limits unchanged.
"""

from __future__ import annotations

import unittest
from typing import Any, Callable

from modules.ai_backend.runtime.device_service import AiDeviceService, _MemoryUserConfig
from modules.ai_backend.runtime.large_models import _test_fixtures as fx
from modules.ai_backend.runtime.large_models.degrade import FloorEntry, FloorKey
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.large_models.tiers import Limits, RamStep, VramStep
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

GIB = 1 << 30


class _FailingUserConfig(_MemoryUserConfig):
    def update(self, mutator: Callable[[dict], None]) -> None:
        raise OSError("disk full")


def _service(user_config: Any) -> AiDeviceService:
    return AiDeviceService(SmallModelManager(), user_config=user_config)


class UserConfigLimitsStoreTests(unittest.TestCase):
    def test_absent_keys_are_defaults(self) -> None:
        store = _service(_MemoryUserConfig()).large_models_limits_store(MemoryLimitsStore())
        self.assertEqual(store.read_limits(), Limits())

    def test_write_touches_only_its_keys_and_none_removes(self) -> None:
        config = _MemoryUserConfig()
        config.config["General"]["theme"] = "dark"
        store = _service(config).large_models_limits_store(MemoryLimitsStore())
        store.write_limits(Limits(vram_cap=8 * GIB, ram_cap=16 * GIB))
        general = config.config["General"]
        self.assertEqual(general["large_models_vram_cap_bytes"], 8 * GIB)
        self.assertEqual(general["large_models_ram_cap_bytes"], 16 * GIB)
        self.assertNotIn("large_models_keep_policy", general)
        self.assertEqual(general["theme"], "dark")
        self.assertEqual(store.read_limits(), Limits(8 * GIB, 16 * GIB))
        store.write_limits(Limits(vram_cap=None, ram_cap=16 * GIB))
        self.assertNotIn("large_models_vram_cap_bytes", general)
        self.assertEqual(store.read_limits(), Limits(None, 16 * GIB))

    def test_retired_keep_policy_is_ignored_once_and_kept(self) -> None:
        config = _MemoryUserConfig()
        config.config["General"]["large_models_keep_policy"] = "host"
        store = _service(config).large_models_limits_store(MemoryLimitsStore())
        with self.assertLogs("modules.ai_backend.runtime.device_service", level="INFO") as captured:
            self.assertEqual(store.read_limits(), Limits())
            store.read_limits()
        self.assertEqual(sum("large_models_keep_policy" in line for line in captured.output), 1)
        store.write_limits(Limits(vram_cap=GIB, ram_cap=None))
        self.assertEqual(config.config["General"]["large_models_keep_policy"], "host")

    def test_invalid_stored_values_read_as_defaults(self) -> None:
        config = _MemoryUserConfig()
        config.config["General"].update({"large_models_vram_cap_bytes": -5, "large_models_ram_cap_bytes": "16"})
        store = _service(config).large_models_limits_store(MemoryLimitsStore())
        with self.assertLogs("modules.ai_backend.runtime.device_service", level="WARNING"):
            self.assertEqual(store.read_limits(), Limits())

    def test_floors_are_delegated(self) -> None:
        config = _MemoryUserConfig()
        floors = MemoryLimitsStore()
        store = _service(config).large_models_limits_store(floors)
        entry = FloorEntry(key=FloorKey("dc", "toy", "big"), vram=VramStep.HALF, ram=RamStep.RUN, reason="device", step="denoise", recorded_at=1.0)
        store.write_floors([entry])
        self.assertEqual(floors.read_floors(), [entry])
        self.assertEqual(store.read_floors(), [entry])
        self.assertEqual(set(config.config["General"]) - {"ai_max_loaded_models"}, set())

    def test_manager_round_trip_and_failed_write(self) -> None:
        config = _MemoryUserConfig()
        service = _service(config)
        arbiter = ResidencyArbiter(admission_wait_s=0.0)
        manager = LargeModelManager(arbiter, limits_store=service.large_models_limits_store(MemoryLimitsStore()), tables=fx.simple_tables())
        manager.set_limits(vram_cap=4 * GIB, ram_cap=None)
        reborn = LargeModelManager(ResidencyArbiter(admission_wait_s=0.0), limits_store=service.large_models_limits_store(MemoryLimitsStore()), tables=fx.simple_tables())
        self.assertEqual(reborn.limits(), Limits(4 * GIB, None))

        failing = LargeModelManager(ResidencyArbiter(admission_wait_s=0.0), limits_store=_service(_FailingUserConfig()).large_models_limits_store(MemoryLimitsStore()), tables=fx.simple_tables())
        with self.assertRaises(OSError):
            failing.set_limits(vram_cap=GIB, ram_cap=None)
        self.assertEqual(failing.limits(), Limits())

    def test_general_section_is_a_copy(self) -> None:
        config = _MemoryUserConfig()
        config.config["General"]["large_models_oom_score_adj"] = False
        section = _service(config).general_section()
        self.assertIs(section["large_models_oom_score_adj"], False)
        section["x"] = 1
        self.assertNotIn("x", config.config["General"])


if __name__ == "__main__":
    unittest.main()
