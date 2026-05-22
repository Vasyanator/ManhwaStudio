# Module: ui_new

## Purpose
This directory contains the legacy Python/Qt 2.x UI. It is not the active application
architecture.

## Architecture
The current application UI, including `CanvasView`, has been fully rewritten in Rust under
`src/`. Do not use files in this directory as references for current canvas, tab, rendering,
state, or performance behavior.

## Contracts and invariants
- Do not edit this directory unless the user explicitly asks to work on the legacy 2.x Python UI.
- Do not inspect `ui_new/canvas_view.py` when analyzing the current `CanvasView`; use
  `src/canvas/` and the Rust tabs under `src/tabs/`.
- New features and fixes belong in the Rust implementation unless the request names this legacy
  UI directly.

## Editing map
- Current canvas architecture: `src/canvas/`.
- Current translation, cleaning, and typing tabs: `src/tabs/translation/`,
  `src/tabs/cleaning/`, and `src/tabs/typing/`.
