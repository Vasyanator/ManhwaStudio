"""
File: modules/ai_backend/inpaint/flux3_klein/test_sampling.py

Purpose:
Unit tests for `flux3_klein/sampling.py`: schedule endpoints, length and the
alpha formula, and the flow-map Euler loop driven by a stub DiT (update rule,
timesteps it sees, step callback, cancellation, refusals).

Notes:
Real torch on the CPU with a recording stub in place of the DiT; skipped where
torch is absent.
"""

from __future__ import annotations

import importlib.util
import unittest

if importlib.util.find_spec("torch") is None or importlib.util.find_spec("PIL") is None:
    raise unittest.SkipTest("torch / PIL не установлены")

import torch
from torch import Tensor

from . import sampling as sm
from .packing import Packed, pack

CHANNELS = {"image": 8, "image_cond": 8, "text": 12}


class _StubDit:
    """Returns a fixed velocity and records what each call saw."""

    def __init__(self, velocity: Tensor) -> None:
        self.velocity = velocity
        self.refs: list[float] = []
        self.image_times: list[float] = []

    def __call__(self, packed: Packed, timesteps_ref: Tensor) -> Tensor:
        self.refs.append(float(timesteps_ref.item()))
        self.image_times.append(float(packed.timesteps["image"][0]))
        return self.velocity


def _packed(samples: int = 1) -> Packed:
    """A pack with `samples` samples of 2 text and 4 float32 image tokens."""
    one = ((torch.zeros(2, 12), torch.zeros(2, 4)), None, (torch.ones(4, 8), torch.zeros(4, 4)))
    return pack([one] * samples, CHANNELS)


class ScheduleTests(unittest.TestCase):
    def test_endpoints_and_length(self) -> None:
        schedule = sm.flow_map_schedule(16, 6.93)
        self.assertEqual(tuple(schedule.shape), (17,))
        self.assertEqual(schedule.dtype, torch.float32)
        self.assertEqual(float(schedule[0]), 1.0)
        self.assertEqual(float(schedule[-1]), 0.0)
        self.assertTrue(bool((schedule[1:] < schedule[:-1]).all()))

    def test_alpha_formula(self) -> None:
        schedule = sm.flow_map_schedule(4, 6.93)
        s = 0.5
        self.assertAlmostEqual(float(schedule[2]), 6.93 * s / (1 + 5.93 * s), places=6)

    def test_alpha_one_is_linear(self) -> None:
        torch.testing.assert_close(sm.flow_map_schedule(4, 1.0), torch.linspace(1.0, 0.0, 5))

    def test_refusals(self) -> None:
        for steps, alpha in ((0, 6.93), (True, 6.93), (4, 0.0), (4, -1.0)):
            with self.subTest(steps=steps, alpha=alpha), self.assertRaises(ValueError):
                sm.flow_map_schedule(steps, alpha)


class DenoiseTests(unittest.TestCase):
    def test_euler_update_with_constant_velocity(self) -> None:
        velocity = torch.full((4, 8), 0.25, dtype=torch.bfloat16)
        dit = _StubDit(velocity)
        packed = _packed()
        schedule = sm.flow_map_schedule(4, 6.93)
        result = sm.denoise_flow_map(dit, packed, schedule, device_type="cpu")
        # Reference numerics: `dt * v` is a bf16 product (v is bf16), added to the
        # float32 state. The dt sum is -1, so the result is close to 1 - 0.25.
        expected = torch.ones(4, 8)
        for step in range(4):
            dt = (schedule[step + 1] - schedule[step]).item()
            expected = expected + dt * velocity
        self.assertEqual(result.dtype, torch.float32)
        torch.testing.assert_close(result, expected, rtol=0, atol=0)
        torch.testing.assert_close(result, torch.full((4, 8), 0.75), rtol=0, atol=1e-3)
        self.assertIs(packed.data["image"], result)
        self.assertEqual(dit.refs, [float(t) for t in schedule[1:]])
        expected_times = [float(torch.tensor(float(t)).to(torch.bfloat16)) for t in schedule[:-1]]
        self.assertEqual(dit.image_times, expected_times)

    def test_step_callback_and_cancellation(self) -> None:
        calls: list[tuple[int, int]] = []
        sm.denoise_flow_map(
            _StubDit(torch.zeros(4, 8)), _packed(), sm.flow_map_schedule(3, 6.93), device_type="cpu",
            step_callback=lambda done, total: calls.append((done, total)),
        )
        self.assertEqual(calls, [(1, 3), (2, 3), (3, 3)])

        class Cancelled(Exception):
            pass

        def cancel(done: int, total: int) -> None:
            if done == 2:
                raise Cancelled

        dit = _StubDit(torch.zeros(4, 8))
        with self.assertRaises(Cancelled):
            sm.denoise_flow_map(dit, _packed(), sm.flow_map_schedule(5, 6.93), device_type="cpu", step_callback=cancel)
        self.assertEqual(len(dit.refs), 2)

    def test_refusals(self) -> None:
        dit = _StubDit(torch.zeros(4, 8))
        with self.assertRaises(ValueError):
            sm.denoise_flow_map(dit, _packed(), torch.ones(1), device_type="cpu")
        with self.assertRaises(ValueError):
            sm.denoise_flow_map(dit, _packed(), torch.ones(2, 2), device_type="cpu")
        with self.assertRaises(ValueError):
            sm.denoise_flow_map(dit, _packed(2), sm.flow_map_schedule(2, 6.93), device_type="cpu")


if __name__ == "__main__":
    unittest.main()
