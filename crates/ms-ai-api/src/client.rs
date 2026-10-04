/*
File: crates/ms-ai-api/src/client.rs

Purpose:
The authenticated `genai::Client` for one service and the blocking bridge that runs a
`genai` future to completion from a worker thread. Native only (`genai` / `tokio` are not
compiled for wasm).

Key functions:
- build_client()
- block_on()
*/

use std::future::Future;
use std::sync::Arc;

use genai::resolver::{AuthData, AuthResolver};
use genai::{Client, ModelIden};

use crate::service::AiApiService;

/// Builds a `genai` client that authenticates with `api_key`, but ONLY for requests whose
/// model resolves to `service`'s adapter; any other adapter gets no credentials, so a key
/// can never leak to a different provider.
#[must_use]
pub fn build_client(service: AiApiService, api_key: String) -> Client {
    let api_key = Arc::new(api_key);
    let expected_adapter = service.adapter_kind();
    let auth_resolver = AuthResolver::from_resolver_fn(
        move |model_iden: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
            if model_iden.adapter_kind == expected_adapter {
                Ok(Some(AuthData::from_single((*api_key).clone())))
            } else {
                Ok(None)
            }
        },
    );
    Client::builder().with_auth_resolver(auth_resolver).build()
}

/// Runs `future` to completion on a fresh multi-thread tokio runtime (all drivers enabled),
/// built for this call and dropped afterwards. It BLOCKS the calling thread: call it only on
/// a worker thread, never on the GUI thread and never from inside a tokio runtime (tokio
/// panics on a nested `block_on`).
///
/// # Errors
/// The `io::Error` of runtime creation; the future's own result is returned untouched.
pub fn block_on<F: Future>(future: F) -> std::io::Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    Ok(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::block_on;

    #[test]
    fn block_on_returns_future_output() {
        let output = block_on(async { 40 + 2 });
        assert!(matches!(output, Ok(42)));
    }
}
