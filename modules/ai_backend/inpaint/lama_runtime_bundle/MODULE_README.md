# Module: modules/ai_backend/inpaint/lama_runtime_bundle

## Purpose
Vendored subset of the LaMa research code (`saicinpainting`, from the original LaMa / big-lama
repository) — the model and pipeline code behind the LaMa V2 inpainting service. This is third-party
code kept in-tree, not project-authored code: prefer minimal, clearly marked edits, and keep new
backend logic in the service layer above.

Only the inference-side subset is vendored. Training entry points, datasets, losses, and evaluation
scripts from upstream are deliberately absent; `training/` here exists because the checkpoint loader
and the generator modules live under that upstream path, not because training is supported.

## Architecture
This bundle is **not imported as part of the `ai_backend` package**. It is reached only through the
standalone runtime module:

```
inpaint/lama.py                     — service: resolves paths, owns the model lifecycle
   │  importlib.util.spec_from_file_location  (loaded BY PATH, not imported)
   ▼
inpaint/lama_v2_runtime_inpainter.py — standalone module; inserts this directory on sys.path
   │  plain `import saicinpainting...`
   ▼
lama_runtime_bundle/saicinpainting/  — this directory
```

`inpaint/lama.py::_prepare_runtime_paths` puts this directory on `sys.path` before the standalone
module is executed, which is what makes the bare `import saicinpainting` inside it resolve. Nothing
else in the backend may rely on that `sys.path` entry: it is a side effect of loading LaMa V2, it
only exists once that has happened, and code inside the package must never `import saicinpainting`
directly.

The entry points the runtime module actually uses are `training.trainers.load_checkpoint`,
`evaluation.utils.move_to_device`, `evaluation.refinement.refine_predict` and
`evaluation.data.pad_tensor_to_modulo`.

## Files and submodules
- `saicinpainting/training/trainers/`: checkpoint loading and the inpainting trainer wrapper that
  wraps the generator into a callable model.
- `saicinpainting/training/modules/`: the generator architecture (FFC-based ResNet and pix2pixhd
  lineage) plus its building blocks.
- `saicinpainting/evaluation/`: inference-time helpers — device movement, modulo padding, the
  refinement (multi-scale high-resolution) pass, and mask utilities.
- `saicinpainting/utils.py`: assorted upstream helpers used by the above.

## Contracts and invariants
- **Keep the divergence from upstream minimal and marked.** There are currently no
  ManhwaStudio-specific edits in this tree; if one becomes unavoidable, comment it in place so the
  next re-vendoring can reapply it.
- **This directory must move together with `inpaint/lama_v2_runtime_inpainter.py` and
  `inpaint/lama.py`.** Both locations are derived from `__file__` in `lama.py` precisely so the three
  cannot drift apart; a wrong path here raises nothing at import time and only fails the first time a
  user runs LaMa V2 inpainting.
- The bundle has no `MODULE_README`-level API of its own: the supported surface is exactly the four
  entry points listed above, used by the standalone runtime module.
- It is torch-dependent and must never be imported at backend startup — only inside the lazy LaMa V2
  model load.

## Editing map
- To change how the checkpoint is loaded or the model is driven: edit
  `inpaint/lama_v2_runtime_inpainter.py`, not this tree.
- To change service-level behaviour (parameters, masks, tiling, encoding): edit `inpaint/lama.py`.
- To update the vendored code: re-copy the corresponding upstream subset and re-check the four entry
  points above still exist with the same signatures.
