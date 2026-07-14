#!/usr/bin/env python3
"""Fail if `egui-docs/` was generated from different crate versions than Cargo.lock pins.

Purpose:
`egui-docs/` is only trustworthy while it describes the egui we actually
compile against. This guard makes a silent drift — someone bumps egui in
Cargo.toml and never regenerates the docs — a loud, early failure instead of a
future agent reading a reference for a version that is no longer in the build.

Wired into the `cargo egui-docs-check` alias (see `.cargo/config.toml`).

Exit codes:
    0 — every crate in egui-docs/VERSION matches Cargo.lock
    1 — drift, or a missing/unreadable input
"""

from __future__ import annotations

import logging
import re
import sys
from pathlib import Path

log = logging.getLogger("check_sync")

REPO_ROOT = Path(__file__).resolve().parents[2]

LOCK = REPO_ROOT / "Cargo.lock"
STAMP = REPO_ROOT / "egui-docs" / "VERSION"


def lock_versions(lock_text: str, names: set[str]) -> dict[str, str]:
    """Extract `name = version` for the requested crates from Cargo.lock.

    Parses the `[[package]]` stanzas directly rather than pulling in a TOML
    dependency; the lock file's shape is stable and this keeps the guard
    dependency-free so it can run in any checkout.
    """
    found: dict[str, str] = {}
    stanza_name: str | None = None
    for line in lock_text.splitlines():
        line = line.strip()
        if line == "[[package]]":
            stanza_name = None
            continue
        m = re.match(r'^name = "([^"]+)"$', line)
        if m:
            stanza_name = m.group(1)
            continue
        m = re.match(r'^version = "([^"]+)"$', line)
        if m and stanza_name in names:
            found[stanza_name] = m.group(1)
    return found


def stamp_versions(stamp_text: str) -> dict[str, str]:
    """Parse `name=version` lines from egui-docs/VERSION, ignoring comments."""
    out: dict[str, str] = {}
    for line in stamp_text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, version = line.partition("=")
        if version:
            out[name.strip()] = version.strip()
    return out


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(message)s")

    if not STAMP.is_file():
        log.error(
            "ERROR egui-docs: version stamp is missing.\n"
            "  Expected: %s\n"
            "  Cause: egui-docs/ has never been generated in this checkout.\n"
            "  Fix: tools/egui_docs/build.sh",
            STAMP,
        )
        return 1
    if not LOCK.is_file():
        log.error(
            "ERROR egui-docs: Cargo.lock is missing.\n"
            "  Expected: %s\n"
            "  Fix: run cargo check to generate it.",
            LOCK,
        )
        return 1

    stamped = stamp_versions(STAMP.read_text(encoding="utf-8"))
    if not stamped:
        log.error(
            "ERROR egui-docs: version stamp is empty.\n"
            "  Path: %s\n"
            "  Fix: tools/egui_docs/build.sh",
            STAMP,
        )
        return 1

    locked = lock_versions(LOCK.read_text(encoding="utf-8"), set(stamped))

    drift: list[str] = []
    for name, want in sorted(stamped.items()):
        have = locked.get(name)
        if have is None:
            drift.append(f"  {name}: documented {want}, but absent from Cargo.lock")
        elif have != want:
            drift.append(f"  {name}: documented {want}, Cargo.lock has {have}")

    if drift:
        log.error(
            "ERROR egui-docs is out of sync with Cargo.lock.\n"
            "%s\n"
            "  Cause: an egui-family crate was upgraded without regenerating the docs.\n"
            "  Effect: agents would read a reference for a version we no longer build "
            "against — exactly the failure egui-docs/ exists to prevent.\n"
            "  Fix: tools/egui_docs/build.sh, then review and commit the egui-docs/ diff.\n"
            "       Hand-written pages (00-version-map.md and friends) must be re-checked "
            "against the new source too; the generator cannot do that for you.",
            "\n".join(drift),
        )
        return 1

    log.info("egui-docs is in sync with Cargo.lock (%d crates checked)", len(stamped))
    return 0


if __name__ == "__main__":
    sys.exit(main())
