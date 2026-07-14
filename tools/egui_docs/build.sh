#!/usr/bin/env bash
#
# File: build.sh
#
# Purpose:
# Regenerate `egui-docs/api/` (the machine-extracted half of the egui
# reference) from the egui-family crates this workspace actually depends on.
#
# Run this after bumping egui/eframe in Cargo.toml. `tools/egui_docs/check_sync.py`
# fails the build if you forget.
#
# Requirements:
# - the `nightly` rust toolchain (rustdoc JSON is nightly-only)
# - python3
#
# Notes:
# `--no-deps` keeps this to the six egui-family crates; documenting the whole
# workspace would drag in the native deps (onnxruntime loader, keyring, rfd)
# and the winresource build script for no benefit.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if ! rustup toolchain list | grep -q '^nightly'; then
    echo "ERROR tools/egui_docs/build.sh" >&2
    echo "Missing the nightly toolchain, which rustdoc JSON requires." >&2
    echo "Fix: rustup toolchain install nightly" >&2
    exit 1
fi

echo "==> building rustdoc JSON for the egui family (nightly)"
RUSTDOCFLAGS="-Z unstable-options --output-format json" \
    cargo +nightly doc \
    -p egui -p eframe -p epaint -p emath -p ecolor -p egui_extras \
    --no-deps

echo "==> generating egui-docs/api/"
python3 tools/egui_docs/gen_api_index.py \
    --json-dir target/doc \
    --out egui-docs/api \
    --stamp egui-docs/VERSION

echo "==> verifying the stamp matches Cargo.lock"
python3 tools/egui_docs/check_sync.py

echo "==> done. Review the diff in egui-docs/ before committing."
