#!/usr/bin/env python3
"""Generate the EasyOCR script-table fixture from an installed easyocr library.

File: tools/model_pins/make_easyocr_fixture.py

Purpose:
    Writes ``crates/ms-sysprobe/fixtures/easyocr_scripts.json``: a mirror of the installed
    easyocr ``config.py`` (craft detector, per-script recognizers, language lists), of the
    recognizer choice in ``easyocr.Reader.__init__`` (easyocr 1.7.2 ``easyocr.py:111-167``) and
    a set of language-list cases. The Rust test of ``crates/ms-sysprobe/src/ai_models/easyocr.rs``
    asserts its hand-written table equals this fixture. Offline developer tool; never on the
    runtime path.

Usage::

    python3 -I -B tools/model_pins/make_easyocr_fixture.py --venv venv \
        --out crates/ms-sysprobe/fixtures/easyocr_scripts.json [--easyocr-version 1.7.2]

Key functions:
    find_easyocr()   locate ``easyocr/config.py`` and the installed version in ``<venv>``.
    build_fixture()  the fixture document from the loaded ``config.py`` namespace.
    main()           CLI; writes only ``--out``.

Notes:
    ``config.py`` is EXECUTED (``runpy``): its recognizer table is built by calls that read the
    library's ``character/*.txt`` files, so it cannot be parsed literally. That is the installed
    pinned library, not downloaded data. The output is byte-deterministic for a given library
    (insertion order of the library dicts plus the fixed lists below). ``CHAIN`` and ``CASES``
    mirror ``Reader.__init__`` by hand: re-read that code whenever the pinned version changes.
"""

from __future__ import annotations

import argparse
import json
import logging
import runpy
import sys
from pathlib import Path
from typing import Any

log = logging.getLogger("make_easyocr_fixture")

RELEASE_BASE_URL = "https://github.com/JaidedAI/EasyOCR/releases/download"
PINNED_VERSION = "1.7.2"
LANG_LIST_NAMES = ("latin_lang_list", "arabic_lang_list", "bengali_lang_list", "cyrillic_lang_list",
                   "devanagari_lang_list", "other_lang_list")

#: ``(script, generation, model key, compatible langs | list name | None)`` in the exact order of
#: the if-chain in ``easyocr.Reader.__init__`` (1.7.2, easyocr.py:111-167). A ``str`` third-item
#: compatible value names a language list from config.py; ``None`` is the latin fallback branch.
CHAIN: tuple[tuple[str, str, str, list[str] | str | None], ...] = (
    ("english", "gen2", "english_g2", ["en"]),
    ("thai", "gen1", "thai_g1", ["th", "en"]),
    ("chinese_tra", "gen1", "zh_tra_g1", ["ch_tra", "en"]),
    ("chinese_sim", "gen2", "zh_sim_g2", ["ch_sim", "en"]),
    ("japanese", "gen2", "japanese_g2", ["ja", "en"]),
    ("korean", "gen2", "korean_g2", ["ko", "en"]),
    ("tamil", "gen1", "tamil_g1", ["ta", "en"]),
    ("telugu", "gen2", "telugu_g2", ["te", "en"]),
    ("kannada", "gen2", "kannada_g2", ["kn", "en"]),
    ("bengali", "gen1", "bengali_g1", "bengali_lang_list"),
    ("arabic", "gen1", "arabic_g1", "arabic_lang_list"),
    ("devanagari", "gen1", "devanagari_g1", "devanagari_lang_list"),
    ("cyrillic", "gen2", "cyrillic_g2", "cyrillic_lang_list"),
    ("latin", "gen2", "latin_g2", None),
)

#: Language lists and the script (or ``error:<kind>``) the Rust selector must give for them.
CASES: tuple[tuple[list[str], str], ...] = (
    (["en"], "english"), (["en", "de"], "latin"), (["de"], "latin"), (["ko"], "korean"), (["ko", "en"], "korean"),
    (["ja"], "japanese"), (["ch_sim", "en"], "chinese_sim"), (["ch_tra"], "chinese_tra"), (["th"], "thai"),
    (["ta"], "tamil"), (["te"], "telugu"), (["kn"], "kannada"), (["bn", "as"], "bengali"), (["mni"], "error:unsupported"),
    (["hi", "sa"], "error:unsupported"),
    (["ar", "fa", "en"], "arabic"), (["hi", "mr"], "devanagari"), (["ru", "uk", "en"], "cyrillic"), (["mn"], "cyrillic"),
    (["vi", "rs_latin"], "latin"),
    (["en", "th"], "thai"),
    (["ko", "ja"], "error:incompatible"), (["ru", "de"], "error:incompatible"), (["ch_sim", "ch_tra"], "error:incompatible"),
    (["xx"], "error:unsupported"), (["ko", "zz"], "error:unsupported"),
)


class FixtureError(Exception):
    """The venv has no usable easyocr, or its tables do not have the expected shape."""


def find_easyocr(venv: Path) -> tuple[Path, str]:
    """``(config.py path, installed version)`` of easyocr inside ``venv``.

    Raises ``FixtureError`` if easyocr or its dist-info is missing, or several are installed.
    """
    configs = sorted(venv.glob("lib/python*/site-packages/easyocr/config.py"))
    configs += sorted(venv.glob("Lib/site-packages/easyocr/config.py"))
    if len(configs) != 1:
        raise FixtureError(f"expected exactly one easyocr/config.py under {venv}, found {len(configs)}")
    site = configs[0].parent.parent
    metas = sorted(site.glob("easyocr-*.dist-info/METADATA"))
    if len(metas) != 1:
        raise FixtureError(f"expected exactly one easyocr dist-info in {site}, found {len(metas)}")
    for line in metas[0].read_text(encoding="utf-8").splitlines():
        if line.startswith("Version:"):
            return configs[0], line.split(":", 1)[1].strip()
    raise FixtureError(f"no Version line in {metas[0]}")


def _archive(url: str) -> str:
    """Release-relative archive path (``<tag>/<name>.zip``) of an official release URL."""
    prefix = RELEASE_BASE_URL + "/"
    if not url.startswith(prefix):
        raise FixtureError(f"model URL {url} is not under {RELEASE_BASE_URL}")
    return url[len(prefix):]


def build_fixture(cfg: dict[str, Any], config_path: Path, version: str) -> dict[str, Any]:
    """The fixture document; ``cfg`` is the executed ``config.py`` namespace."""
    try:
        recognition = cfg["recognition_models"]
        lists = {name: cfg[name] for name in LANG_LIST_NAMES}
        detector = cfg["detection_models"]["craft"]
        all_langs = cfg["all_lang_list"]
        scripts = []
        for script, generation, model, compatible in CHAIN:
            entry = recognition[generation][model]
            scripts.append({
                "script": script,
                "model": model,
                "model_script": entry["model_script"],
                "filename": entry["filename"],
                "md5": entry["md5sum"],
                "archive": _archive(entry["url"]),
                "compatible": lists[compatible] + ["en"] if isinstance(compatible, str) else compatible,
            })
        detector_row = {"filename": detector["filename"], "md5": detector["md5sum"], "archive": _archive(detector["url"])}
    except KeyError as exc:
        raise FixtureError(f"easyocr {version} config.py lacks expected key {exc}") from exc
    char_dir = config_path.parent / "character"
    # Listed in all_lang_list but shipped without a character file: Reader fails on them.
    unusable = [lang for lang in all_langs if not (char_dir / f"{lang}_char.txt").exists()]
    return {
        "_comment": "Mirror of easyocr " + version + " config.py (detector, recognizers, language lists) and of the recognizer choice in easyocr.Reader.__init__ (easyocr.py:111-167). Owner: crates/ms-sysprobe/src/ai_models/easyocr.rs (Rust test: table == fixture). Generated from the installed library; regenerate when the pinned easyocr version changes.",
        "easyocr_version": version,
        "release_base_url": RELEASE_BASE_URL,
        "detector": detector_row,
        "lang_lists": {name[: -len("_lang_list")]: value for name, value in lists.items()},
        "unusable_langs": unusable,
        "scripts": scripts,
        "cases": [{"langs": langs, "expect": expect} for langs, expect in CASES],
    }


def main(argv: list[str] | None = None) -> int:
    """CLI entry. Exit 0 = written, 2 = missing/unexpected library (nothing written)."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--venv", type=Path, required=True, help="virtualenv holding the pinned easyocr")
    parser.add_argument("--out", type=Path, required=True, help="fixture path to write")
    parser.add_argument("--easyocr-version", default=PINNED_VERSION, help="required installed version")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    try:
        config_path, version = find_easyocr(args.venv)
        if version != args.easyocr_version:
            raise FixtureError(f"installed easyocr is {version}, required {args.easyocr_version}")
        cfg = runpy.run_path(str(config_path))
        document = build_fixture(cfg, config_path, version)
    except FixtureError as exc:
        log.error("%s", exc)
        return 2
    args.out.write_text(json.dumps(document, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    log.info("wrote %s (easyocr %s, %d scripts, %d cases)", args.out, version, len(document["scripts"]), len(CASES))
    return 0


if __name__ == "__main__":
    sys.exit(main())
