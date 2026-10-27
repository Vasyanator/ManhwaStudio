"""
File: modules/ai_backend/inpaint/flux2_klein/test_prompt_cache_golden.py

Purpose:
GOLDEN characterization of FLUX.2 klein's prompt-library surface, recorded
BEFORE the prompt-cache orchestration moved into the model-agnostic
`runtime/prompt_cache_service.py`. It is the "old behaviour" reference of that
extraction: every wire answer and every refusal text of `.status`
(`prompt_cached`, `prompt_cache_entry`), `.prompt_cache.build` (new from
memory, already present, rename), `.list`, `.export` and `.import` (a v2 file
of its own library, a foreign v2 file, a v1 file migrated, a v1 file refused,
junk), plus the one-time v1 migration report a listing carries.

Main responsibilities:
- `_compute_golden` - one fixed scenario over a temporary model tree and a
  temporary program root, every answer normalized (`_normalize`);
- `PromptCacheGoldenTests` - today's answers against `fixtures/prompt_cache_golden.json`;
- `--write-golden` - rewrites the snapshot from the current code
  (`python -m modules.ai_backend.inpaint.flux2_klein.test_prompt_cache_golden --write-golden`
  from the repository root). A refactor must NOT need it: a diff here is a
  behaviour change and is reviewed as one.

Notes:
- Normalization: the temp tree root becomes `<root>` (its bare name, part of
  an encoder label, `<root-name>`); `pl.utc_now` is patched
  to `CREATED_AT`; every positive `size_bytes` becomes `"<size>"`, because a
  built entry records the encoder path (`text_encoder_path` metadata), so its
  byte size follows the length of the temp directory's name (the export answer
  additionally records whether its size equals the copied file's).
- The device is pinned to `cpu` (`hardware._resolve_selected_backend_device`),
  so a build answer does not depend on the test host's GPU.
- Torch-free: the tensor (de)serialization is stubbed by `_LibraryCase`.
- Nothing is written outside the temporary directory, except by the explicit
  `--write-golden` opt-in.
"""

from __future__ import annotations

import argparse
import json
import sys
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware, prompt_cache
from modules.ai_backend.runtime import prompt_library as pl

from .test_prompt_cache import LIB_4B, LIB_9B, _LibraryCase, _v1_metadata, _write_prompt_cache_file
from ._test_fixtures import load_9b_config

#: The golden snapshot file. Tracked; regenerate only through `--write-golden`.
GOLDEN_PATH = Path(__file__).resolve().parent / "fixtures" / "prompt_cache_golden.json"

#: The `created_at` every entry and memory embedding of the scenario carries.
CREATED_AT = "2026-10-27T00:00:00Z"


class _GoldenTree(_LibraryCase):
    """The scenario's environment: `_LibraryCase`'s tree, temp program root and stubbed tensors, driven by hand (never collected: no test method)."""

    def _scenario(self) -> None:
        """Exists only so the case can be instantiated with a method name."""


def _normalize(value: Any, root: str) -> Any:
    """`value` with the temp root replaced by `<root>` (and its bare directory name, which encoder labels carry, by `<root-name>`) in every string, and every positive `size_bytes` replaced by `"<size>"` (module notes)."""
    if isinstance(value, dict):
        return {key: ("<size>" if key == "size_bytes" and isinstance(item, int) and item > 0 else _normalize(item, root)) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_normalize(item, root) for item in value]
    if isinstance(value, str):
        return value.replace(root, "<root>").replace(Path(root).name, "<root-name>")
    return value


def _refusal(call: Any) -> dict[str, str]:
    """`{type, message}` of the `ValueError` `call()` raises; an answer instead of a refusal is recorded as `{"answered": ...}`."""
    try:
        answer = call()
    except ValueError as exc:
        return {"type": type(exc).__name__, "message": str(exc)}
    return {"answered": answer}


def _status(case: _GoldenTree, params: dict[str, object]) -> dict[str, Any]:
    """The prompt-cache part of `.status`."""
    out = case.service.status(params)
    return {key: out[key] for key in ("prompt_cached", "prompt_cache_entry", "text_encoder_available")}


def _run_scenario(case: _GoldenTree) -> dict[str, Any]:
    """The fixed scenario; every step's answer in order (see the module docstring)."""
    steps: dict[str, Any] = {}
    # A v1 family present from the start: `.status` never migrates it, the
    # first library operation (the build below) does, and the listing reports it.
    legacy = case.program_root / "prompt_cache" / "qwen-te-ffffffff" / "legacy preset.msprompt"
    _write_prompt_cache_file(legacy, _v1_metadata(prompt="legacy text"), shape=[1, 512, 12288])
    steps["status_empty"] = _status(case, case.lib_params(prompt="a cat"))
    steps["legacy_after_status"] = legacy.exists()
    case.seed_current("a cat")
    steps["status_memory"] = _status(case, case.lib_params(prompt="a cat"))
    steps["build_new_from_memory"] = case.service.prompt_cache_build(case.lib_params(prompt="a cat"), name="Кот")
    steps["legacy_after_build"] = legacy.exists()
    steps["build_already"] = case.service.prompt_cache_build(case.lib_params(prompt="a cat"))
    steps["build_rename"] = case.service.prompt_cache_build(case.lib_params(prompt="a cat"), name="  Котик\n")
    steps["status_library_or_memory"] = _status(case, case.lib_params(prompt="a cat"))
    other = case.add_entry("a dog", source="uncensored", fmt="q8_0", name="Пёс", created_at="2026-09-01T00:00:00Z")
    case.add_entry("a dog", source="stock", fmt="q4_k_m", created_at="2026-08-01T00:00:00Z")
    case.add_entry("short", msl=256, created_at="2026-07-01T00:00:00Z")
    case.add_entry("", created_at="2026-06-01T00:00:00Z")
    steps["status_other_source"] = _status(case, case.lib_params(prompt="a dog", text_encoder_source="uncensored"))
    steps["status_preferred_stock"] = _status(case, case.lib_params(prompt="a dog", text_encoder_source="stock"))
    steps["status_without_encoder"] = _status(case, case.without_encoder(prompt="a dog"))
    case.declare_distilled(False)
    steps["status_guidance_needs_empty"] = _status(case, case.lib_params(prompt="a dog", guidance_scale=4.0))
    case.declare_distilled(True)
    (prompt_cache.library_of(LIB_9B).directory / "junk.msprompt").write_bytes(b"junk")
    steps["list_stock"] = case.service.prompt_cache_list(case.lib_params(text_encoder_source="stock"))
    steps["list_256"] = case.service.prompt_cache_list(case.lib_params(max_sequence_length=256))
    steps["list_lenient_4b"] = case.service.prompt_cache_list({"prompt_library": LIB_4B})
    dest = case.root / "out" / "copy.msprompt"
    dest.parent.mkdir()
    exported = case.service.prompt_cache_export(case.lib_params(), other.file, str(dest))
    steps["export"] = exported
    steps["export_size_matches_file"] = exported["size_bytes"] == dest.stat().st_size
    v2_own = case.root / "outside" / "own.msprompt"
    _write_prompt_cache_file(v2_own, pl.build_metadata(prompt_cache.flux2_library_spec(LIB_4B), prompt="imported", name="Imp", max_sequence_length=512, encoder=pl.EncoderProvenance("stock", "bf16", False, "e" * 64, "te"), origin=pl.ORIGIN_BUILT), shape=[1, 512, 8])
    steps["import_v2_other_library"] = case.service.prompt_cache_import(case.lib_params(), str(v2_own))
    steps["import_v2_again_matching"] = case.service.prompt_cache_import(case.lib_params(prompt_library=LIB_4B), str(v2_own))
    v2_foreign = case.root / "outside" / "foreign.msprompt"
    _write_prompt_cache_file(v2_foreign, pl.build_metadata(pl.LibrarySpec("other_model", "prompt_embeds", "bfloat16", "BF16"), prompt="x", name="", max_sequence_length=512, encoder=pl.EncoderProvenance("stock", "bf16", False, "e" * 64, "te"), origin=pl.ORIGIN_BUILT), shape=[1, 512, 8])
    steps["import_v2_foreign"] = _refusal(lambda: case.service.prompt_cache_import(case.lib_params(), str(v2_foreign)))
    v1_ok = case.root / "outside" / "my preset.msprompt"
    _write_prompt_cache_file(v1_ok, _v1_metadata(), shape=[1, 512, 12288])
    steps["import_v1_migrated"] = case.service.prompt_cache_import(case.lib_params(), str(v1_ok))
    folder = case.root / "text_encoder"
    (folder / "config.json").write_text(json.dumps(load_9b_config("uncensored_text_encoder"), indent=2), encoding="utf-8")
    unsalted = svc.text_encoder_fingerprint(str(folder), rope_salt=False)
    v1_old = case.root / "outside" / "old.msprompt"
    _write_prompt_cache_file(v1_old, _v1_metadata(text_encoder_id=unsalted, prompt="old"), shape=[1, 512, 12288])
    steps["import_v1_refused"] = _refusal(lambda: case.service.prompt_cache_import(case.lib_params(), str(v1_old)))
    junk = case.root / "junk.msprompt"
    junk.write_bytes(b"junk")
    steps["import_junk"] = _refusal(lambda: case.service.prompt_cache_import(case.lib_params(), str(junk)))
    unmarked = case.root / "unmarked.msprompt"
    _write_prompt_cache_file(unmarked, {"format": "something"})
    steps["import_unmarked"] = _refusal(lambda: case.service.prompt_cache_import(case.lib_params(), str(unmarked)))
    steps["import_relative"] = _refusal(lambda: case.service.prompt_cache_import(case.lib_params(), "relative.msprompt"))
    steps["build_without_library"] = _refusal(lambda: case.service.prompt_cache_build(case.params(prompt="a cat")))
    steps["build_empty_prompt"] = _refusal(lambda: case.service.prompt_cache_build(case.lib_params(prompt="  ")))
    steps["build_without_encoder"] = _refusal(lambda: case.service.prompt_cache_build(case.without_encoder(prompt="a new text")))
    steps["build_bad_library_id"] = _refusal(lambda: case.service.prompt_cache_build(case.lib_params(prompt="a cat", prompt_library="Bad-Id")))
    steps["list_without_library"] = _refusal(lambda: case.service.prompt_cache_list({}))
    steps["list_bad_order"] = _refusal(lambda: case.service.prompt_cache_list({"prompt_library": LIB_9B, "prompt_cache_precision_order": ["bf16", "bf16"]}))
    steps["export_missing_file"] = _refusal(lambda: case.service.prompt_cache_export(case.lib_params(), "missing.msprompt", str(case.root / "x.msprompt")))
    steps["export_without_library"] = _refusal(lambda: case.service.prompt_cache_export({}, other.file, str(case.root / "x.msprompt")))
    steps["list_final"] = case.service.prompt_cache_list(case.lib_params(text_encoder_source="stock"))
    steps["memory_texts"] = sorted([remembered.library, remembered.text, remembered.max_sequence_length, remembered.entry.file if remembered.entry is not None else ""] for remembered in case.service._memory_index.values())
    return steps


def _compute_golden() -> dict[str, Any]:
    """Today's answers of the scenario, normalized, as the JSON document."""
    case = _GoldenTree("_scenario")
    case.setUp()
    try:
        with patch.object(pl, "utc_now", return_value=CREATED_AT), patch.object(hardware, "_resolve_selected_backend_device", lambda _fallback: "cpu"):
            steps = _run_scenario(case)
        return {
            "_note": (
                "GOLDEN snapshot of FLUX.2 klein's prompt-library answers (test_prompt_cache_golden.py). "
                "A refactor must keep it byte-identical; regenerate only for a deliberate behaviour change with "
                "`python -m modules.ai_backend.inpaint.flux2_klein.test_prompt_cache_golden --write-golden` and review the diff."
            ),
            "steps": _normalize(steps, str(case.root)),
        }
    finally:
        case.doCleanups()


class PromptCacheGoldenTests(unittest.TestCase):
    """GOLDEN: today's prompt-library answers and refusal texts; a diff is a behaviour change."""

    maxDiff = None

    def test_the_answers_match_the_snapshot(self) -> None:
        golden = json.loads(GOLDEN_PATH.read_text(encoding="utf-8"))
        current = json.loads(json.dumps(_compute_golden(), ensure_ascii=False))
        self.assertEqual(set(current["steps"]), set(golden["steps"]))
        for name, answer in golden["steps"].items():
            with self.subTest(step=name):
                self.assertEqual(current["steps"][name], answer)


def _main(argv: list[str]) -> int:
    """`--write-golden`: rewrite `fixtures/prompt_cache_golden.json` from the current code."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1] if __doc__ else None)
    parser.add_argument("--write-golden", action="store_true", help="rewrite the golden snapshot file")
    args = parser.parse_args(argv)
    if not args.write_golden:
        parser.error("nothing to do: pass --write-golden (tests run under pytest)")
    GOLDEN_PATH.parent.mkdir(parents=True, exist_ok=True)
    GOLDEN_PATH.write_text(json.dumps(_compute_golden(), indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"written {GOLDEN_PATH}")  # noqa: T201 - CLI output of a developer tool
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
