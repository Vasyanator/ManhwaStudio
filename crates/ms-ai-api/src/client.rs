/*
File: crates/ms-ai-api/src/client.rs

Purpose:
The authenticated `genai::Client` for one `AiApiTarget` (service + optional base URL) and the
blocking bridge that runs a `genai` future to completion from a worker thread. Native only
(`genai` / `tokio` are not compiled for wasm).

Key functions:
- build_client()
- block_on()
*/

use std::future::Future;
use std::sync::Arc;

use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use crate::target::AiApiTarget;

/// Builds a `genai` client for `target` that authenticates with `api_key`, but ONLY for
/// requests whose model resolves to the target service's adapter; any other adapter gets no
/// credentials, so a key can never leak to a different provider. For the target's adapter the
/// resolver always answers (an empty `api_key` is sent as an empty key, which a keyless
/// compatible server ignores), so `genai` never falls back to an `*_API_KEY` environment
/// variable. A compatible target additionally routes that adapter's requests to its base URL.
#[must_use]
pub fn build_client(target: &AiApiTarget, api_key: String) -> Client {
    let api_key = Arc::new(api_key);
    let expected_adapter = target.service().adapter_kind();
    let auth_resolver = AuthResolver::from_resolver_fn(
        move |model_iden: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
            if model_iden.adapter_kind == expected_adapter {
                Ok(Some(AuthData::from_single((*api_key).clone())))
            } else {
                Ok(None)
            }
        },
    );
    let builder = Client::builder().with_auth_resolver(auth_resolver);
    let Some(endpoint) = target.endpoint() else {
        return builder.build();
    };
    let endpoint: Arc<str> = Arc::from(endpoint);
    let target_resolver = ServiceTargetResolver::from_resolver_fn(
        move |mut service_target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            if service_target.model.adapter_kind == expected_adapter {
                service_target.endpoint = Endpoint::from_owned(Arc::clone(&endpoint));
            }
            Ok(service_target)
        },
    );
    builder.with_service_target_resolver(target_resolver).build()
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
    use super::{block_on, build_client};
    use crate::model_id::model_iden;
    use crate::service::AiApiService;
    use crate::target::AiApiTarget;

    #[test]
    fn block_on_returns_future_output() {
        let output = block_on(async { 40 + 2 });
        assert!(matches!(output, Ok(42)));
    }

    /// Resolves the service target `genai` would call for `model` (no network I/O).
    fn resolved_endpoint(target: &AiApiTarget, model: &str) -> Option<String> {
        let client = build_client(target, String::new());
        let iden = model_iden(target.service(), model).ok()?;
        let resolved = block_on(async move { client.resolve_service_target(iden).await }).ok()?.ok()?;
        Some(resolved.endpoint.base_url().to_string())
    }

    #[test]
    fn compatible_target_routes_requests_to_its_base_url() {
        let openai = AiApiTarget::new(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080").ok();
        let anthropic = AiApiTarget::new(AiApiService::AnthropicCompatible, "http://127.0.0.1:8080/v1").ok();
        assert_eq!(openai.and_then(|target| resolved_endpoint(&target, "local-model")).as_deref(), Some("http://127.0.0.1:8080/v1/"));
        assert_eq!(anthropic.and_then(|target| resolved_endpoint(&target, "local-model")).as_deref(), Some("http://127.0.0.1:8080/v1/"));
    }

    #[test]
    fn hosted_target_keeps_the_official_endpoint() {
        let target = AiApiTarget::new(AiApiService::OpenAi, "http://127.0.0.1:8080").ok();
        assert_eq!(target.and_then(|target| resolved_endpoint(&target, "gpt-4o-mini")).as_deref(), Some("https://api.openai.com/v1/"));
    }
}
