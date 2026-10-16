"""
File: modules/ai_backend/inpaint/flux3_klein/test_phases.py

Purpose:
Unit tests for `flux3_klein/phases.py`: the begin/end bracket, the `failed`
flag, `None` observer, and that observer exceptions propagate.
"""

from __future__ import annotations

import unittest
from collections.abc import Mapping
from typing import Any

from .phases import Phase, observed


class ObservedTests(unittest.TestCase):
    def setUp(self) -> None:
        self.events: list[tuple[Phase, str, dict[str, Any]]] = []

    def _observer(self, phase: Phase, event: str, info: Mapping[str, Any]) -> None:
        self.events.append((phase, event, dict(info)))

    def test_a_successful_body_is_bracketed(self) -> None:
        with observed(self._observer, Phase.DENOISE, steps=16):
            self.events.append((Phase.DENOISE, "body", {}))
        self.assertEqual(
            self.events,
            [
                (Phase.DENOISE, "begin", {"steps": 16}),
                (Phase.DENOISE, "body", {}),
                (Phase.DENOISE, "end", {"steps": 16, "failed": False}),
            ],
        )

    def test_a_failing_body_reports_failed_and_propagates(self) -> None:
        with self.assertRaises(RuntimeError):
            with observed(self._observer, Phase.DIT_LOAD):
                raise RuntimeError("out of memory")
        self.assertEqual(self.events[-1], (Phase.DIT_LOAD, "end", {"failed": True}))

    def test_no_observer_observes_nothing(self) -> None:
        with observed(None, Phase.ENCODE, tokens=80):
            pass
        with self.assertRaises(KeyError):
            with observed(None, Phase.ENCODE):
                raise KeyError("body")

    def test_an_observer_exception_on_begin_skips_the_body(self) -> None:
        ran: list[bool] = []

        def explode(phase: Phase, event: str, info: Mapping[str, Any]) -> None:
            raise ValueError(f"observer broke on {event}")

        with self.assertRaises(ValueError):
            with observed(explode, Phase.DECODE):
                ran.append(True)
        self.assertEqual(ran, [])

    def test_an_observer_exception_on_end_propagates(self) -> None:
        def explode_on_end(phase: Phase, event: str, info: Mapping[str, Any]) -> None:
            if event == "end":
                raise ValueError("observer broke on end")

        with self.assertRaises(ValueError):
            with observed(explode_on_end, Phase.AE_LOAD):
                pass

    def test_phases_are_in_run_order(self) -> None:
        self.assertEqual(
            [phase.name for phase in Phase],
            [
                "DIT_LOAD",
                "AE_LOAD",
                "DIT_OFFLOAD",
                "ENCODER_LOAD",
                "ENCODE",
                "ENCODER_RELEASE",
                "REF_ENCODE",
                "DIT_RESTORE",
                "DENOISE",
                "DIT_OFFLOAD_DECODE",
                "DECODE",
            ],
        )


if __name__ == "__main__":
    unittest.main()
