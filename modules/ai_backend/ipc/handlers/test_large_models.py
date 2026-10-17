"""
File: modules/ai_backend/ipc/handlers/test_large_models.py

Purpose:
Unit tests for the `large_models.*` IPC handlers (`large_models.py`), called
directly through the registry with a `SimpleNamespace` `AppState` holding a REAL
`LargeModelManager` over a real `ResidencyArbiter` and fake budget probes. The
model is the core's header-only toy (`runtime/large_models/_test_fixtures.py`):
safetensors headers in a temp directory, no torch, no weights, no GPU.

Coverage:
- every method is registered and listed in `ALL_METHODS`;
- `profiles`: the adapter seam (`large_model_inputs` -> options, device; the
  optional `large_model_name` -> the registration planned; a composing
  `large_model_profiles` adapter answers itself), the
  `shape` -> `RequestShape` and `vram_step` / `ram_step` -> `Steps` mappings
  (absent = `auto`), the two-axis answer shape, the wire answer is
  JSON-serializable, and invalid requests (the retired `profile` included) are
  refused;
- `limits.set`: absent keeps, `null` clears, invalid values and the retired
  `keep_policy` are refused and leave the limits unchanged; the store sees the
  write; two concurrent partial sets lose no cap and leave the store and the
  limits in force equal;
- `status` / `unload` / `learned.reset` answer their documented shapes;
- the `profiles` answer's key tree and value types equal the Rust client's
  pinned fixture (`crates/ms-local-ai/fixtures/large_models_profiles_v13.json`,
  parsed by `ms_local_ai::large_models::parse_profiles`), so the two sides
  cannot drift apart silently;
- the `config.py` defaults tree matches ms-config for the large-model keys:
  `large_models_oom_score_adj` defaults to `true`, the two caps are absent.
"""

from __future__ import annotations

import dataclasses
import json
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from modules.ai_backend.ipc import protocol
from modules.ai_backend.ipc.registry import HandlerContext, get_handler
from modules.ai_backend.runtime.large_models import _test_fixtures as fx
from modules.ai_backend.runtime.large_models.degrade import FloorEntry, FloorKey
from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from modules.ai_backend.runtime.large_models.spec import RequestShape
from modules.ai_backend.runtime.large_models.tiers import AUTO_STEPS, Limits, RamStep, Steps, VramStep
from modules.ai_backend.runtime.memory_budget import DeviceBudget, HostBudget
from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

GIB = 1 << 30

#: The Rust side of the `large_models.profiles` shape parity (tracked, repo-relative).
RUST_PROFILES_FIXTURE = Path(__file__).resolve().parents[4] / "crates" / "ms-local-ai" / "fixtures" / "large_models_profiles_v13.json"

METHODS = (
    protocol.METHOD_LARGE_MODELS_PROFILES,
    protocol.METHOD_LARGE_MODELS_LIMITS_SET,
    protocol.METHOD_LARGE_MODELS_STATUS,
    protocol.METHOD_LARGE_MODELS_UNLOAD,
    protocol.METHOD_LARGE_MODELS_LEARNED_RESET,
)


class _Probes:
    """A 32 GiB card with 24 GiB free and 64 GiB of available RAM."""

    def host(self) -> HostBudget:
        return HostBudget(total=128 * GIB, available=64 * GIB, cgroup_limit=None, cgroup_used=None, rss_anon=GIB, rss_shmem=0, reserve=2 * GIB)

    def device(self, device: str) -> DeviceBudget:
        return DeviceBudget(device=device, total=32 * GIB, free=24 * GIB, torch_allocated=0, torch_reserved=0, non_torch=8 * GIB, reserve=GIB // 2, cap=None)


class _Adapter:
    """The model service's side of the seam: records the params it was given."""

    def __init__(self) -> None:
        self.params: list[dict[str, Any]] = []

    def large_model_inputs(self, params: dict[str, Any]) -> tuple[Any, str]:
        if params.get("bad"):
            raise ValueError("bad params")
        self.params.append(params)
        return {"options": True}, "cuda:0"


class _NamingAdapter(_Adapter):
    """An adapter whose params select one of several registrations (FLUX.2's dtype variants)."""

    def large_model_name(self, params: dict[str, Any]) -> str:
        return "toy.variant" if params.get("variant") else "toy"


class _ComposingAdapter(_Adapter):
    """A multi-handle adapter that composes the whole `profiles` answer itself."""

    def __init__(self) -> None:
        super().__init__()
        self.composed: list[tuple[dict[str, Any], RequestShape, str | None, Steps]] = []

    def large_model_inputs(self, params: dict[str, Any]) -> tuple[Any, str]:
        raise AssertionError("a composing adapter's large_model_inputs is not called")

    def large_model_profiles(self, params: dict[str, Any], shape: RequestShape, *, format: str | None, steps: Steps) -> dict[str, Any]:
        self.composed.append((params, shape, format, steps))
        return {"model": "composed"}


class _GatedStore(MemoryLimitsStore):
    """A store whose FIRST write blocks until `release` is set (the writes it saw are recorded)."""

    def __init__(self) -> None:
        super().__init__()
        self.entered = threading.Event()
        self.release = threading.Event()
        self.second_entered = threading.Event()
        self.writes: list[Limits] = []

    def write_limits(self, limits: Limits) -> None:
        self.writes.append(limits)
        if len(self.writes) == 1:
            self.entered.set()
            if not self.release.wait(10.0):
                raise AssertionError("the gated store was never released")
        else:
            self.second_entered.set()
        super().write_limits(limits)


class _Rig:
    def __init__(self, test: unittest.TestCase, *, store: MemoryLimitsStore | None = None, adapter: _Adapter | None = None) -> None:
        directory = tempfile.TemporaryDirectory()
        test.addCleanup(directory.cleanup)
        root = Path(directory.name)
        files = {fmt: fx.toy_files(root / fmt, fmt) for fmt in ("big", "small")}
        spec = dataclasses.replace(fx.toy_spec(files), formats=lambda options: fx.toy_formats())
        self.store = store if store is not None else MemoryLimitsStore()
        self.manager = LargeModelManager(ResidencyArbiter(_Probes(), admission_wait_s=0.0), limits_store=self.store, tables=fx.simple_tables(), probes=_Probes(), device_class=lambda device: "test_class")
        self.manager.register(spec, model="toy")
        self.manager.register(spec, model="toy.variant")
        self.adapter = adapter if adapter is not None else _Adapter()
        self.state = SimpleNamespace(large_models=self.manager, large_model_adapters={"toy": self.adapter})

    def call(self, method: str, header: dict[str, Any]) -> dict[str, Any]:
        ctx = HandlerContext(state=self.state, events=None, get_health_snapshot=dict)
        fields, blob = get_handler(method)(ctx, header, b"", threading.Event())
        self.assert_blob_empty(blob)
        return fields

    @staticmethod
    def assert_blob_empty(blob: bytes) -> None:
        if blob:
            raise AssertionError(f"large_models.* never answers a blob, got {len(blob)} bytes")


SHAPE = {"region_width": 1024, "region_height": 768, "references": [{"w": 512, "h": 512}], "prompt_tokens": 120}


class RegistrationTests(unittest.TestCase):
    def test_every_method_is_registered_and_listed(self) -> None:
        for method in METHODS:
            self.assertIn(method, protocol.ALL_METHODS)
            self.assertIsNotNone(get_handler(method), method)


class ProfilesTests(unittest.TestCase):
    def test_profiles_forwards_and_answers_both_axes(self) -> None:
        rig = _Rig(self)
        answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "params": {"path": "/x"}, "shape": SHAPE})
        self.assertEqual(rig.adapter.params, [{"path": "/x"}])
        self.assertEqual(answer["format"], "big")
        self.assertEqual([entry["id"] for entry in answer["vram"]["steps"]], [step.value for step in VramStep if step is not VramStep.AUTO])
        self.assertEqual([entry["id"] for entry in answer["ram"]["steps"]], [step.value for step in RamStep if step is not RamStep.AUTO])
        for key in ("selected", "vram", "ram", "learned_floor", "limits", "formats", "suggestions"):
            self.assertIn(key, answer)
        self.assertNotIn("profiles", answer)
        selected = answer["selected"]
        self.assertEqual((selected["vram_requested"], selected["ram_requested"]), ("auto", "auto"))
        # Everything fits the 24 GiB toy budget, so «Авто» on both axes resolves to the fastest pair.
        self.assertTrue(selected["fits"])
        self.assertEqual((selected["vram"], selected["ram"]), ("max", "keep"))
        self.assertTrue(set(answer["limits"]).issuperset({"vram_cap", "ram_cap", "vram_total", "vram_free", "ram_total", "ram_free"}))
        self.assertNotIn("keep_policy", answer["limits"])
        json.dumps(answer)

    def test_profiles_matches_the_manager_for_the_mapped_request(self) -> None:
        rig = _Rig(self)
        cases = (
            ({}, AUTO_STEPS),
            ({"vram_step": None, "ram_step": None}, AUTO_STEPS),
            ({"vram_step": "half"}, Steps(VramStep.HALF, RamStep.AUTO)),
            ({"vram_step": "min", "ram_step": "run"}, Steps(VramStep.MIN, RamStep.RUN)),
            ({"ram_step": "min"}, Steps(VramStep.AUTO, RamStep.MIN)),
        )
        for extra, steps in cases:
            with self.subTest(extra=extra):
                answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "shape": SHAPE, "format": "small", **extra})
                direct = rig.manager.profiles("toy", {"options": True}, RequestShape(1024, 768, ((512, 512),), 120), device="cuda:0", format="small", steps=steps).to_dict()
                self.assertEqual(answer, direct)
                self.assertEqual((answer["selected"]["vram_requested"], answer["selected"]["ram_requested"]), (steps.vram.value, steps.ram.value))

    def test_large_model_name_selects_the_registration_planned(self) -> None:
        rig = _Rig(self, adapter=_NamingAdapter())
        planned: list[str] = []
        profiles = rig.manager.profiles

        def recording(model: str, *args: Any, **kwargs: Any) -> Any:
            planned.append(model)
            return profiles(model, *args, **kwargs)

        rig.manager.profiles = recording  # type: ignore[method-assign]
        answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "params": {"variant": True}, "shape": SHAPE})
        self.assertEqual(planned, ["toy.variant"])
        self.assertEqual(answer["model"], "toy.variant")
        answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "params": {}, "shape": SHAPE})
        self.assertEqual(planned, ["toy.variant", "toy"])
        self.assertEqual(answer["model"], "toy")

    def test_a_composing_adapter_answers_itself(self) -> None:
        adapter = _ComposingAdapter()
        rig = _Rig(self, adapter=adapter)
        answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "params": {"p": 1}, "shape": SHAPE, "format": "small", "ram_step": "min"})
        self.assertEqual(answer, {"model": "composed"})
        self.assertEqual(adapter.composed, [({"p": 1}, RequestShape(1024, 768, ((512, 512),), 120), "small", Steps(VramStep.AUTO, RamStep.MIN))])

    def test_a_learned_floor_is_reported_per_axis(self) -> None:
        floor = FloorEntry(key=FloorKey("test_class", "toy", "big"), vram=VramStep.HALF, ram=None, reason="device", step="denoise", recorded_at=1.0)
        rig = _Rig(self, store=MemoryLimitsStore(floors=[floor]))
        answer = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "shape": SHAPE})
        self.assertEqual((answer["vram"]["floor"], answer["ram"]["floor"]), ("half", None))
        self.assertEqual((answer["learned_floor"]["vram"], answer["learned_floor"]["ram"]), ("half", None))
        self.assertEqual([entry["id"] for entry in answer["vram"]["steps"] if entry["below_floor"]], ["max", "phase"])
        self.assertFalse(any(entry["below_floor"] for entry in answer["ram"]["steps"]))

    def test_invalid_requests_are_refused(self) -> None:
        rig = _Rig(self)
        cases = [
            {"shape": SHAPE},
            {"model": "unknown", "shape": SHAPE},
            {"model": "toy"},
            {"model": "toy", "shape": {"region_width": True, "region_height": 4}},
            {"model": "toy", "shape": {"region_width": 4}},
            {"model": "toy", "shape": {**SHAPE, "references": [{"w": 0, "h": 4}]}},
            {"model": "toy", "shape": {**SHAPE, "references": {"w": 4}}},
            {"model": "toy", "shape": {**SHAPE, "prompt_tokens": -1}},
            {"model": "toy", "shape": SHAPE, "profile": "economy"},
            {"model": "toy", "shape": SHAPE, "profile": None},
            {"model": "toy", "shape": SHAPE, "vram_step": "turbo"},
            {"model": "toy", "shape": SHAPE, "ram_step": "keep_forever"},
            {"model": "toy", "shape": SHAPE, "vram_step": 1},
            {"model": "toy", "shape": SHAPE, "ram_step": ""},
            {"model": "toy", "shape": SHAPE, "vram_step": "keep"},
            {"model": "toy", "shape": SHAPE, "params": []},
            {"model": "toy", "shape": SHAPE, "params": {"bad": True}},
        ]
        for header in cases:
            with self.subTest(header=header), self.assertRaises(ValueError):
                rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, header)


class LimitsTests(unittest.TestCase):
    def test_absent_keeps_null_clears(self) -> None:
        rig = _Rig(self)
        answer = rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, {"vram_cap_bytes": 8 * GIB})
        self.assertEqual(answer, {"limits": {"vram_cap": 8 * GIB, "ram_cap": None}})
        answer = rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, {"ram_cap_bytes": 16 * GIB})
        self.assertEqual(answer["limits"], {"vram_cap": 8 * GIB, "ram_cap": 16 * GIB})
        answer = rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, {"vram_cap_bytes": None})
        self.assertEqual(answer["limits"], {"vram_cap": None, "ram_cap": 16 * GIB})
        self.assertEqual(rig.store.read_limits(), Limits(None, 16 * GIB))

    def test_concurrent_partial_sets_lose_no_cap(self) -> None:
        store = _GatedStore()
        rig = _Rig(self, store=store)
        answers: dict[str, dict[str, Any]] = {}

        def call(name: str, header: dict[str, Any]) -> None:
            answers[name] = rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, header)

        first = threading.Thread(target=call, args=("vram", {"vram_cap_bytes": 8 * GIB}))
        first.start()
        self.assertTrue(store.entered.wait(10.0))
        second = threading.Thread(target=call, args=("ram", {"ram_cap_bytes": 16 * GIB}))
        second.start()
        # The second set may not merge (nor write) while the first one is mid-write.
        self.assertFalse(store.second_entered.wait(0.2))
        store.release.set()
        first.join(10.0)
        second.join(10.0)
        self.assertFalse(first.is_alive() or second.is_alive())
        both = Limits(8 * GIB, 16 * GIB)
        self.assertEqual(store.writes, [Limits(8 * GIB, None), both])
        self.assertEqual(rig.manager.limits(), both)
        self.assertEqual(store.read_limits(), both)
        self.assertEqual(answers["vram"]["limits"], {"vram_cap": 8 * GIB, "ram_cap": None})
        self.assertEqual(answers["ram"]["limits"], {"vram_cap": 8 * GIB, "ram_cap": 16 * GIB})

    def test_invalid_values_change_nothing(self) -> None:
        rig = _Rig(self, store=MemoryLimitsStore(Limits(GIB, None)))
        for header in ({"vram_cap_bytes": -1}, {"ram_cap_bytes": "1"}, {"vram_cap_bytes": True}, {"keep_policy": "host"}, {"keep_policy": None}, {"vram_cap_bytes": 2 * GIB, "keep_policy": "drop"}):
            with self.subTest(header=header), self.assertRaises(ValueError):
                rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, header)
        self.assertEqual(rig.manager.limits(), Limits(GIB, None))
        self.assertEqual(rig.store.read_limits(), Limits(GIB, None))


class StatusUnloadResetTests(unittest.TestCase):
    def test_status_of_an_empty_pool(self) -> None:
        rig = _Rig(self)
        self.assertEqual(rig.call(protocol.METHOD_LARGE_MODELS_STATUS, {}), {"large_models": [], "limits": {"vram_cap": None, "ram_cap": None}})

    def test_unload_of_an_empty_pool(self) -> None:
        rig = _Rig(self)
        self.assertEqual(rig.call(protocol.METHOD_LARGE_MODELS_UNLOAD, {}), {"unloaded": []})
        self.assertEqual(rig.call(protocol.METHOD_LARGE_MODELS_UNLOAD, {"model": "toy"}), {"unloaded": []})
        with self.assertRaises(ValueError):
            rig.call(protocol.METHOD_LARGE_MODELS_UNLOAD, {"model": 5})

    def test_learned_reset_counts_cleared_floors(self) -> None:
        floors = [
            FloorEntry(key=FloorKey("test_class", "toy", "big"), vram=VramStep.HALF, ram=None, reason="device", step="denoise", recorded_at=1.0),
            FloorEntry(key=FloorKey("test_class", "other", "big"), vram=None, ram=RamStep.RUN, reason="host", step="decode", recorded_at=2.0),
        ]
        rig = _Rig(self, store=MemoryLimitsStore(floors=floors))
        self.assertEqual(rig.call(protocol.METHOD_LARGE_MODELS_LEARNED_RESET, {"model": "toy"}), {"cleared": 1})
        self.assertEqual(rig.call(protocol.METHOD_LARGE_MODELS_LEARNED_RESET, {"model": None}), {"cleared": 1})
        self.assertEqual(rig.store.read_floors(), [])


def _shape_mismatches(python: Any, rust: Any, path: str = "$") -> list[str]:
    """Where the Python answer's key tree / value types differ from the Rust fixture's.

    Dicts must have the same keys. A `null` on either side matches any type (the
    field is nullable; which side is populated depends on the plan). An int
    where the fixture has a float is accepted (Rust reads an integer as `f64`);
    the reverse is a mismatch. Lists compare their first elements when both
    sides have one.
    """
    if python is None or rust is None:
        return []
    if isinstance(rust, dict):
        if not isinstance(python, dict):
            return [f"{path}: object expected, got {type(python).__name__}"]
        found = [f"{path}: keys differ: python-only {sorted(set(python) - set(rust))}, rust-only {sorted(set(rust) - set(python))}"] if set(python) != set(rust) else []
        for key in sorted(set(python) & set(rust)):
            found.extend(_shape_mismatches(python[key], rust[key], f"{path}.{key}"))
        return found
    if isinstance(rust, list):
        if not isinstance(python, list):
            return [f"{path}: array expected, got {type(python).__name__}"]
        return _shape_mismatches(python[0], rust[0], f"{path}[0]") if python and rust else []
    if isinstance(rust, bool) or isinstance(python, bool):
        return [] if isinstance(rust, bool) and isinstance(python, bool) else [f"{path}: bool vs {type(python).__name__}"]
    if isinstance(rust, float) and isinstance(python, (int, float)):
        return []
    return [] if type(python) is type(rust) else [f"{path}: {type(rust).__name__} expected, got {type(python).__name__}"]


class RustFixtureShapeTests(unittest.TestCase):
    """The `profiles` wire shape against the Rust client's pinned fixture (`ProfilesReport.to_dict` is the shape owner)."""

    def _answers(self) -> list[dict[str, Any]]:
        floor = FloorEntry(key=FloorKey("test_class", "toy", "big"), vram=VramStep.HALF, ram=None, reason="device_oom", step="denoise", recorded_at=1.0)
        rig = _Rig(self, store=MemoryLimitsStore(Limits(None, 32 * GIB), floors=[floor]))
        fitting = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "shape": SHAPE})
        # A 1 KiB VRAM cap refuses every device step: `reason`, `shortest_resource`
        # and the `suggestions` are populated on this answer.
        rig.call(protocol.METHOD_LARGE_MODELS_LIMITS_SET, {"vram_cap_bytes": 1024})
        refused = rig.call(protocol.METHOD_LARGE_MODELS_PROFILES, {"model": "toy", "shape": SHAPE, "vram_step": "max", "ram_step": "keep"})
        # The FLUX.2 adapter adds `encoder`: the encoder handle's `selected`
        # (`Flux2KleinInpaintService.large_model_profiles`), same shape.
        return [{**answer, "encoder": answer["selected"]} for answer in (fitting, refused)]

    def test_profiles_shape_matches_rust_fixture(self) -> None:
        rust = json.loads(RUST_PROFILES_FIXTURE.read_text(encoding="utf-8"))
        answers = self._answers()
        for index, answer in enumerate(answers):
            with self.subTest(answer=index):
                self.assertEqual(_shape_mismatches(json.loads(json.dumps(answer)), rust), [])
        # The comparison is only as strong as what got populated: every nullable
        # part of the fixture is non-null in at least one answer.
        self.assertTrue(any(answer["learned_floor"] is not None for answer in answers))
        self.assertTrue(any(answer["suggestions"] for answer in answers))
        self.assertTrue(any(answer["selected"]["reason"] is not None for answer in answers))
        self.assertTrue(any(answer["selected"]["needs_reclaim"] is not None for answer in answers))
        self.assertTrue(all(answer["selected"]["forecast"] is not None and answer["selected"]["plan_summary"] for answer in answers))
        self.assertTrue(all(answer["formats"] and answer["formats"][0]["fits_best"] is not None for answer in answers[:1]))


class UserConfigDefaultsTests(unittest.TestCase):
    """`config.py` `USER_CONFIG_DEFAULTS` agrees with Rust `ms_config::user_config_defaults` on the large-model keys."""

    def test_the_oom_switch_defaults_on_and_the_caps_are_absent(self) -> None:
        import config

        general = config.USER_CONFIG_DEFAULTS["General"]
        self.assertIs(general["large_models_oom_score_adj"], True)
        # Absent = no cap; the backend is the caps' only writer (`device_service.py`).
        self.assertNotIn("large_models_vram_cap_bytes", general)
        self.assertNotIn("large_models_ram_cap_bytes", general)


if __name__ == "__main__":
    unittest.main()
