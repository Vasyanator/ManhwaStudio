/*
FILE OVERVIEW: src/tabs/translation/machine_translation.rs
Background machine-translation controller for Translation tab.

Main types:
- `MtService`: translation provider selector (`Google` + `Yandex` + `DeepL`).
- `MtTranslateItem` / `MtTranslateRequest`: per-bubble input and batch request payload.
- `MtControllerEvent`: UI-facing worker events.
- `TranslationMtController`: MT run lifecycle with immediate cancel semantics.
- `ActiveMtRun`: currently active run-thread metadata (`run_id`, cancel flag, handle).

Runtime model:
- each MT run is executed in its own background thread (GUI thread is never blocked);
- `request_cancel` marks run cancelled and detaches the thread immediately;
- detached/stale run events are ignored by `run_id` and never touch canvas state.

Backend helper:
- `translate_texts_via_translator` dispatches into `machine_translators` backends.
*/

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use super::machine_translators::MachineTranslatorBackend;
use super::machine_translators::deepl::DeeplMtBackend;
use super::machine_translators::google::GoogleMtBackend;
use super::machine_translators::yandex::YandexMtBackend;

const MT_EVENT_POLL_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum MtService {
    Google,
    Yandex,
    Deepl,
}

impl MtService {
    pub fn key(self) -> &'static str {
        match self {
            MtService::Google => "google",
            MtService::Yandex => "yandex",
            MtService::Deepl => "deepl",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            MtService::Google => "Google",
            MtService::Yandex => "Yandex",
            MtService::Deepl => "DeepL",
        }
    }

    pub fn from_key(raw: &str) -> Option<Self> {
        let key = raw.trim().to_ascii_lowercase();
        match key.as_str() {
            "google" => Some(MtService::Google),
            "yandex" => Some(MtService::Yandex),
            "deepl" => Some(MtService::Deepl),
            _ => None,
        }
    }

    pub fn all() -> &'static [Self] {
        &[MtService::Google, MtService::Yandex, MtService::Deepl]
    }
}

#[derive(Debug, Clone)]
pub struct MtTranslateItem {
    pub bubble_id: i64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct MtTranslateRequest {
    pub service: MtService,
    pub source_lang: String,
    pub target_lang: String,
    pub items: Vec<MtTranslateItem>,
}

#[derive(Debug, Clone)]
pub enum MtControllerEvent {
    RunStarted {
        total: usize,
    },
    ItemTranslated {
        bubble_id: i64,
        translated_text: String,
    },
    ItemFailed {
        bubble_id: i64,
        error: String,
    },
    RunFinished {
        translated: usize,
        errors: usize,
    },
    RunCancelled {
        translated: usize,
        errors: usize,
    },
    RunFailed {
        error: String,
    },
}

#[derive(Debug)]
struct ActiveMtRun {
    run_id: u64,
    cancel_requested: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Debug)]
pub struct TranslationMtController {
    busy: bool,
    next_run_id: u64,
    active_run: Option<ActiveMtRun>,
    detached_run_threads: Vec<JoinHandle<()>>,
    evt_tx: Sender<WorkerEvent>,
    evt_rx: Receiver<WorkerEvent>,
}

impl Default for TranslationMtController {
    fn default() -> Self {
        Self::new()
    }
}

impl TranslationMtController {
    pub fn new() -> Self {
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        Self {
            busy: false,
            next_run_id: 1,
            active_run: None,
            detached_run_threads: Vec::new(),
            evt_tx,
            evt_rx,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn start_translation(&mut self, request: MtTranslateRequest) -> Result<(), String> {
        if self.busy {
            return Err("Перевод уже выполняется.".to_string());
        }
        if request.items.is_empty() {
            return Err("Нет пузырей для перевода.".to_string());
        }

        self.reap_detached_run_threads();
        let run_id = self.next_run_id;
        self.next_run_id = self.next_run_id.saturating_add(1);
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let run_cancel_requested = Arc::clone(&cancel_requested);
        let evt_tx = self.evt_tx.clone();

        let thread = thread::spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                run_translate_request(run_id, request, &evt_tx, &run_cancel_requested);
            }));
            if result.is_err() {
                let _ = evt_tx.send(WorkerEvent::RunFailed {
                    run_id,
                    error: "Worker машинного перевода аварийно завершился.".to_string(),
                });
            }
        });

        self.active_run = Some(ActiveMtRun {
            run_id,
            cancel_requested,
            thread,
        });
        self.busy = true;
        Ok(())
    }

    pub fn request_cancel(&mut self) -> bool {
        if !self.busy {
            return false;
        }
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            run.cancel_requested.store(true, Ordering::Relaxed);
            self.detached_run_threads.push(run.thread);
            return true;
        }
        false
    }

    pub fn poll_events(&mut self) -> Vec<MtControllerEvent> {
        self.reap_detached_run_threads();
        let mut out = Vec::new();

        for _ in 0..MT_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::RunStarted { run_id, total }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::RunStarted { total });
                    }
                }
                Ok(WorkerEvent::ItemTranslated {
                    run_id,
                    bubble_id,
                    translated_text,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::ItemTranslated {
                            bubble_id,
                            translated_text,
                        });
                    }
                }
                Ok(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id,
                    error,
                }) => {
                    if self.is_active_run(run_id) {
                        out.push(MtControllerEvent::ItemFailed { bubble_id, error });
                    }
                }
                Ok(WorkerEvent::RunFinished {
                    run_id,
                    translated,
                    errors,
                }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunFinished { translated, errors });
                    }
                }
                Ok(WorkerEvent::RunCancelled {
                    run_id,
                    translated,
                    errors,
                }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunCancelled { translated, errors });
                    }
                }
                Ok(WorkerEvent::RunFailed { run_id, error }) => {
                    if self.is_active_run(run_id) {
                        self.finish_active_run();
                        out.push(MtControllerEvent::RunFailed { error });
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.finish_active_run();
                    out.push(MtControllerEvent::RunFailed {
                        error: "Worker машинного перевода отключился.".to_string(),
                    });
                    break;
                }
            }
        }

        out
    }

    fn is_active_run(&self, run_id: u64) -> bool {
        self.active_run
            .as_ref()
            .is_some_and(|active| active.run_id == run_id)
    }

    fn finish_active_run(&mut self) {
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            if run.thread.is_finished() {
                let _ = run.thread.join();
            } else {
                self.detached_run_threads.push(run.thread);
            }
        }
    }

    fn reap_detached_run_threads(&mut self) {
        if self.detached_run_threads.is_empty() {
            return;
        }
        let mut still_running = Vec::with_capacity(self.detached_run_threads.len());
        for thread in self.detached_run_threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                still_running.push(thread);
            }
        }
        self.detached_run_threads = still_running;
    }
}

impl Drop for TranslationMtController {
    fn drop(&mut self) {
        self.busy = false;
        if let Some(run) = self.active_run.take() {
            run.cancel_requested.store(true, Ordering::Relaxed);
            self.detached_run_threads.push(run.thread);
        }
        self.reap_detached_run_threads();
    }
}

#[derive(Debug)]
enum WorkerEvent {
    RunStarted {
        run_id: u64,
        total: usize,
    },
    ItemTranslated {
        run_id: u64,
        bubble_id: i64,
        translated_text: String,
    },
    ItemFailed {
        run_id: u64,
        bubble_id: i64,
        error: String,
    },
    RunFinished {
        run_id: u64,
        translated: usize,
        errors: usize,
    },
    RunCancelled {
        run_id: u64,
        translated: usize,
        errors: usize,
    },
    RunFailed {
        run_id: u64,
        error: String,
    },
}

fn run_translate_request(
    run_id: u64,
    request: MtTranslateRequest,
    evt_tx: &Sender<WorkerEvent>,
    cancel_requested: &Arc<AtomicBool>,
) {
    let MtTranslateRequest {
        service,
        source_lang,
        target_lang,
        items,
    } = request;
    let _ = evt_tx.send(WorkerEvent::RunStarted {
        run_id,
        total: items.len(),
    });

    let mut translated = 0usize;
    let mut errors = 0usize;

    for item in items {
        if cancel_requested.load(Ordering::Relaxed) {
            let _ = evt_tx.send(WorkerEvent::RunCancelled {
                run_id,
                translated,
                errors,
            });
            return;
        }

        let item_id = item.bubble_id;
        let backend_result =
            translate_texts_via_translator(service, &source_lang, &target_lang, vec![item.text]);

        if cancel_requested.load(Ordering::Relaxed) {
            let _ = evt_tx.send(WorkerEvent::RunCancelled {
                run_id,
                translated,
                errors,
            });
            return;
        }

        match backend_result {
            Ok(mut results) => {
                if results.len() != 1 {
                    let err = format!(
                        "Некорректный ответ переводчика: ожидался 1 результат, получено {}.",
                        results.len()
                    );
                    let _ = evt_tx.send(WorkerEvent::ItemFailed {
                        run_id,
                        bubble_id: item_id,
                        error: err,
                    });
                    errors += 1;
                    continue;
                }

                match results.pop().expect("len checked == 1") {
                    Ok(text) => {
                        let _ = evt_tx.send(WorkerEvent::ItemTranslated {
                            run_id,
                            bubble_id: item_id,
                            translated_text: text,
                        });
                        translated += 1;
                    }
                    Err(err) => {
                        let _ = evt_tx.send(WorkerEvent::ItemFailed {
                            run_id,
                            bubble_id: item_id,
                            error: err,
                        });
                        errors += 1;
                    }
                }
            }
            Err(err) => {
                let _ = evt_tx.send(WorkerEvent::ItemFailed {
                    run_id,
                    bubble_id: item_id,
                    error: err,
                });
                errors += 1;
            }
        }
    }

    let _ = evt_tx.send(WorkerEvent::RunFinished {
        run_id,
        translated,
        errors,
    });
}

fn translate_texts_via_translator(
    service: MtService,
    source_lang: &str,
    target_lang: &str,
    texts: Vec<String>,
) -> Result<Vec<Result<String, String>>, String> {
    match service {
        MtService::Google => GoogleMtBackend.translate_texts(source_lang, target_lang, texts),
        MtService::Yandex => YandexMtBackend.translate_texts(source_lang, target_lang, texts),
        MtService::Deepl => DeeplMtBackend.translate_texts(source_lang, target_lang, texts),
    }
}
