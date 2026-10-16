"""
File: tools/large_model_calibration/cli.py

Purpose:
Command line of the large-model calibration tool: one subcommand per job.

- `flux3` - measure FLUX.3 klein through the real service (`flux3.py`);
- `flux2` - measure FLUX.2 klein through the real service (`flux2.py`);
- `fit`   - fit `calibration/<device_class>.json` from report files (`fit.py`);
- `check` - forecast-vs-measured acceptance over report files (`fit.py`).

Key functions:
- `main()` - parse and dispatch; returns the process exit code
"""

from __future__ import annotations

import argparse
from pathlib import Path

from . import fit, flux2, flux3


def build_parser() -> tuple[argparse.ArgumentParser, dict[str, argparse.ArgumentParser]]:
    """The top-level parser and each subcommand's parser (help text in English)."""
    parser = argparse.ArgumentParser(
        prog="venv/bin/python tools/large_model_calibration",
        description="Measure large models (FLUX.2 / FLUX.3 klein) per manager step on a real GPU and fit the large-model manager's cost tables.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    parsers = {
        "flux3": flux3.add_parser(subparsers),
        "flux2": flux2.add_parser(subparsers),
        "fit": fit.add_fit_parser(subparsers),
        "check": fit.add_check_parser(subparsers),
    }
    return parser, parsers


def main(argv: list[str], *, repo_root: Path) -> int:
    """Run the tool; returns the process exit code (2 for a usage error)."""
    parser, parsers = build_parser()
    args = parser.parse_args(argv)
    sub = parsers[args.command]
    if args.command == "flux3":
        return flux3.run(sub, args, repo_root=repo_root)
    if args.command == "flux2":
        return flux2.run(sub, args, repo_root=repo_root)
    if args.command == "fit":
        return fit.run_fit(sub, args, repo_root=repo_root)
    return fit.run_check(sub, args)
