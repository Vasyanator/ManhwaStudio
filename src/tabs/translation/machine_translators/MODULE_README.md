# Module: src/tabs/translation/machine_translators

## Purpose
Machine-translation provider implementations used by the Translation tab MT worker.

## Architecture
`mod.rs` defines the `MachineTranslatorBackend` trait: a synchronous batch translation contract
returning one `Result` per input text. `machine_translation.rs` selects a provider from `MtService`
and calls the backend inside a background run thread.

This directory is UI-agnostic. It should not know about `egui`, `CanvasView`, `ProjectData`, or
translation panels. Provider modules may use blocking network clients because they execute only in
MT worker threads.

## Files and submodules
- `mod.rs`: provider modules and `MachineTranslatorBackend` trait.
- `google.rs`: Google Translate provider using the `translators` crate and simple language-code
  normalization.
- `yandex.rs`: Yandex web API provider with per-text language detection for `auto`, Android-like
  request parameters, UCID cache, response parsing, and readable API error mapping.
- `deepl.rs`: DeepL web JSON-RPC provider using client-state initialization, sentence splitting,
  job handling, global request throttling, retry/backoff handling, and language normalization.

## Contracts and invariants
- `translate_texts` must return a vector with exactly one per-item result for each input text when
  the provider-level setup succeeds.
- Empty or whitespace-only input items should return `Ok(String::new())` for that item.
- Provider failures that affect only one bubble should be returned as that item's `Err`; failures
  that prevent the provider from running may return the outer `Err`.
- Backends must not mutate canvas/project/UI state and must not spawn their own UI-visible worker
  lifecycles.
- Blocking HTTP and rate-limit waits are acceptable only because callers run these providers inside
  background MT threads.
- Do not add API keys, credentials, or user secrets here. If a provider later requires credentials,
  thread them through explicit settings and avoid logging secret values.

## Editing map
- To add a provider, implement `MachineTranslatorBackend` in a new file, export it from `mod.rs`,
  add an `MtService` variant and dispatch branch in `translation/machine_translation.rs`, and add
  UI selection text in `panels/machine_translation.rs`.
- To change provider language normalization, edit the provider-specific `normalize_*_lang` helper.
- To change Yandex request behavior or error mapping, edit `yandex.rs`.
- To change DeepL throttling, retry, JSON-RPC flow, or client-state handling, edit `deepl.rs`.
- To change the backend contract shape, update `MachineTranslatorBackend`, all provider
  implementations, and `translation/machine_translation.rs` together.
