/*
FILE OVERVIEW: src/tabs/translation/machine_translators/mod.rs
Machine translation backends for Translation tab worker.

Main items:
- `MachineTranslatorBackend`: common backend contract for batch translation.
- `google` / `yandex` / `deepl`: concrete service implementations.

Design notes:
- This module is UI-agnostic and runs only inside background MT worker thread.
- New providers should implement `MachineTranslatorBackend` and be dispatched
  from `machine_translation.rs` by `MtService`.
*/

pub mod deepl;
pub mod google;
pub mod yandex;

pub trait MachineTranslatorBackend {
    fn translate_texts(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, String>>, String>;
}
