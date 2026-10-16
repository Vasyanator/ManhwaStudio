"""
Package: modules/ai_backend/runtime/residency

The single owner of "who holds which backend memory, which lease is in which
state, and what may be reclaimed for whom": the residency arbiter that both
model pools (small models, large models) take their leases from, as equal
peers. The usable bytes themselves are read by `..memory_budget`; this package
decides only who to evict, park or refuse.

This file deliberately imports nothing and re-exports nothing: every submodule
must stay torch-free at import (the probes import torch lazily), and a
re-export would defeat that the same way it would for the parent `runtime`
package.

Import submodules directly, e.g.
`from ..runtime.residency.arbiter import ResidencyArbiter`.
"""
