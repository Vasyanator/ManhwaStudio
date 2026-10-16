"""
Package: modules/ai_backend/runtime/large_models

The model-agnostic core of the large-model manager: host and device memory
budgets, component/block inventories built from safetensors headers, the
residency tiers and the five resource profiles, the planner that fits a profile
to the budgets, the calibration tables and the memory/time forecast.

Model packages (FLUX.2 klein, FLUX.3 klein) contribute one `spec.LargeModelSpec`
each; nothing in this package names a model family.

This file deliberately imports nothing and re-exports nothing: importing any
submodule must stay torch-free (torch and psutil are imported lazily, only by
the probes that need them), and a re-export would defeat that the same way it
would for the parent `runtime` package.

Import submodules directly, e.g.
`from ..runtime.large_models.planner import plan`.
"""
