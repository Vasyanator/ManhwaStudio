# Module: modules/ai_backend/translate

## Purpose
Machine-translation adapters for the backend. Wraps the third-party `deep_translator` providers
behind one stateless service that backs the `translate.deep` IPC method.

## Architecture
`server.py` (the composition root) constructs one `MachineTranslationService` and stores it as
`AppState.machine_translation`; `ipc/handlers/translate.py` reaches it through
`HandlerContext.state.machine_translation` and never imports this package directly.

The service holds no model and no connection: each `translate_batch` call looks up the provider
class, validates the caller-supplied credentials, constructs a translator, and translates the
segments one by one. Provider classes are imported lazily inside `_deep_translator_classes()`, so a
backend without `deep_translator` still starts and simply reports the service as unavailable.

## Files and submodules
- `__init__.py`: docstring only. Deliberately re-exports nothing so importing the package never
  pulls in `deep_translator`; import `machine_translation` explicitly.
- `machine_translation.py`: `MachineTranslationService` (`health` / `translate_batch`), the
  provider-key → class map, and the per-provider required-credential table
  (`_SERVICE_REQUIRED_FIELDS`). Edit here to add or change a provider.

## Contracts and invariants
- Supported provider keys and their required credentials (`_SERVICE_REQUIRED_FIELDS`): `google`
  (none), `chatgpt` / `yandex` / `deepl` (`api_key`), `microsoft` (`api_key` + `region`). An unknown
  key raises `ValueError` — there is no fallback to Google. An empty/absent `service` is a different
  case: it selects `google` as the documented default, which is not a fallback from a failed lookup.
- `health()` never raises: a missing/broken `deep_translator` yields
  `{"available": False, "error": ...}` so one bad optional dependency cannot break the backend
  health snapshot.
- `translate_batch` returns exactly one entry per input segment, in input order: `{"ok": True,
  "text": str}` or `{"ok": False, "error": str}`. A single failing segment must not abort the batch.
- Blank/whitespace-only segments return an empty translation without a network call.
- Caller `params` are filtered against the translator's `__init__` signature; missing required
  constructor parameters raise `ValueError` before any network call, and a constructor failure
  raises `RuntimeError`.
- The service is stateless and therefore safe to call from several IPC worker threads at once.
- Credentials arrive per request and must never be logged or persisted here.

## Editing map
- Add a provider: extend `_deep_translator_classes()` and `_SERVICE_REQUIRED_FIELDS`, then mirror the
  key in the Rust-side translation settings.
- Change per-segment result shape or error reporting: `MachineTranslationService.translate_batch`.
- Request/response shape of `translate.deep`: `../ipc/handlers/translate.py` and
  `../ipc/PROTOCOL.md`.
