"""
File: modules/ai_backend/test_device_service.py

Purpose:
Unit tests for AI backend device selection contracts.

Main responsibilities:
- verify that `not-selected` remains an unresolved user choice;
- verify that hidden configured flags cannot override the sentinel;
- verify that automatic runtime fallbacks are not persisted as explicit choices.
"""

from __future__ import annotations

import unittest
from unittest.mock import patch

from modules.ai_device import AIDevice
from modules.ai_backend.device_service import _MemoryUserConfig, _OnnxDeviceSelector


class AiDeviceSentinelTests(unittest.TestCase):
    def test_not_selected_ignores_stale_torch_configured_flag(self) -> None:
        user_config = _MemoryUserConfig()
        user_config.config = {
            "General": {
                "ai_device": "not-selected",
                "ai_device_configured": True,
            }
        }

        with patch.object(AIDevice, "detect_available_devices", return_value=["cpu", "cuda"]):
            self.assertFalse(AIDevice.has_configured_device(user_config))
            self.assertTrue(AIDevice.needs_manual_selection(user_config, ["cpu", "cuda"]))

    def test_not_selected_ignores_stale_onnx_configured_flags(self) -> None:
        user_config = _MemoryUserConfig()
        user_config.config = {
            "General": {
                "ai_onnx_provider": "not-selected",
                "ai_onnx_provider_configured": True,
                "ai_onnx_device_id": "not-selected",
                "ai_onnx_device_id_configured": True,
            }
        }
        selector = _OnnxDeviceSelector(user_config)

        with (
            patch.object(
                selector,
                "_detect_available_providers",
                return_value=["DmlExecutionProvider", "CPUExecutionProvider"],
            ),
            patch.object(
                selector,
                "_detect_provider_device_names",
                return_value={"0": "GPU 0", "1": "GPU 1"},
            ),
            patch("modules.ai_backend.device_service.platform.system", return_value="Windows"),
        ):
            state = selector.get_state()

        self.assertEqual(state["selected_onnx_provider"], "DmlExecutionProvider")
        self.assertEqual(state["selected_onnx_device_id"], "0")
        self.assertTrue(state["onnx_device_needs_selection"])
        self.assertEqual(user_config.config["General"]["ai_onnx_provider"], "not-selected")
        self.assertEqual(user_config.config["General"]["ai_onnx_device_id"], "not-selected")

    def test_single_runtime_onnx_fallback_is_not_marked_configured(self) -> None:
        user_config = _MemoryUserConfig()
        user_config.config = {
            "General": {
                "ai_onnx_provider": "not-selected",
                "ai_onnx_device_id": "not-selected",
            }
        }
        selector = _OnnxDeviceSelector(user_config)

        with (
            patch.object(
                selector,
                "_detect_available_providers",
                return_value=["CPUExecutionProvider"],
            ),
            patch.object(
                selector,
                "_detect_provider_device_names",
                return_value={"0": "CPU"},
            ),
        ):
            state = selector.get_state()

        self.assertEqual(state["selected_onnx_provider"], "CPUExecutionProvider")
        self.assertEqual(state["selected_onnx_device_id"], "0")
        self.assertFalse(state["onnx_device_needs_selection"])
        self.assertNotIn(
            "ai_onnx_provider_configured",
            user_config.config["General"],
        )
        self.assertNotIn(
            "ai_onnx_device_id_configured",
            user_config.config["General"],
        )

    def test_single_directml_device_still_requires_confirmation(self) -> None:
        user_config = _MemoryUserConfig()
        user_config.config = {
            "General": {
                "ai_onnx_provider": "not-selected",
                "ai_onnx_device_id": "not-selected",
            }
        }
        selector = _OnnxDeviceSelector(user_config)

        with (
            patch.object(
                selector,
                "_detect_available_providers",
                return_value=["DmlExecutionProvider", "CPUExecutionProvider"],
            ),
            patch.object(
                selector,
                "_detect_provider_device_names",
                return_value={"0": "GPU 0"},
            ),
            patch("modules.ai_backend.device_service.platform.system", return_value="Windows"),
        ):
            state = selector.get_state()

        self.assertEqual(state["selected_onnx_provider"], "DmlExecutionProvider")
        self.assertEqual(state["selected_onnx_device_id"], "0")
        self.assertTrue(state["onnx_device_needs_selection"])

    def test_windows_gpu_name_probe_falls_back_to_wmic(self) -> None:
        user_config = _MemoryUserConfig()
        selector = _OnnxDeviceSelector(user_config)

        def fake_run_command(cmd: list[str], timeout: float = 1.0) -> str:
            self.assertLessEqual(timeout, 1.0)
            if cmd[0] == "powershell":
                return ""
            if cmd[0] == "wmic":
                return "Name\nNVIDIA RTX Laptop GPU\nAMD Radeon Graphics\n"
            return ""

        with (
            patch("modules.ai_backend.device_service.shutil.which") as which,
            patch.object(selector, "_run_command", side_effect=fake_run_command),
        ):
            which.side_effect = lambda exe: exe if exe in {"powershell", "wmic"} else None
            names = selector._detect_windows_gpu_names_by_order()

        self.assertEqual(
            names,
            {
                "0": "NVIDIA RTX Laptop GPU",
                "1": "AMD Radeon Graphics",
            },
        )


if __name__ == "__main__":
    unittest.main()
