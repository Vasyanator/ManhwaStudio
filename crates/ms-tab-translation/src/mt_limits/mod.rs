//! Process-wide request-rate limiting of the machine-translation services (Google, DeepL,
//! Yandex): pacing per lane (a service over one transport: built-in HTTP or the browser
//! extension), one request in flight per lane, per-route cooldowns after a rate limit or block,
//! cancellable reported waits, and the long-text split for request size limits.
//!
//! GUI-free and provider-agnostic: providers take an `MtPermit` from an `MtCall` before EVERY
//! HTTP request and finish it with a typed `MtSignal`; no provider sleeps or throttles itself.
//! Map and contracts: `MODULE_README.md` next to this file.

// Submodules are `pub(crate)` and named by path (`mt_limits::call::MtCall`): the map in
// `MODULE_README.md` says which file owns which contract.
pub(crate) mod call;
pub(crate) mod limiter;
pub(crate) mod registry;
pub(crate) mod split;
pub(crate) mod table;
#[cfg(test)]
pub(crate) mod test_support;
