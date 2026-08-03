//! Rust-native effect shell and provider wire adapters for RustProviderKit.
//!
//! `rust-provider-kit-core` owns values and pure transitions. This crate owns HTTP,
//! SSE, credential coordination, provider dialects, cancellation, retry, and
//! lifecycle supervision. Browser and loopback effects are in
//! `rust-provider-kit-platform`.

mod account_supervisor;
mod adapter;
mod adapters;
mod codex_version;
mod credential_contract;
mod execution_session;
mod execution_supervisor;
mod http_transport;
mod in_memory_credential_store;
mod oauth_replay;
mod openrouter_oauth;
mod registration_session;
mod registry;
mod reqwest_transport;
mod runtime;
mod secure_file;
mod sse;
mod wire;

pub use in_memory_credential_store::InMemoryProviderCredentialStore;
pub use openrouter_oauth::OpenRouterOAuthRegistrationRequest;
pub use runtime::ProviderRuntime;

#[cfg(test)]
mod tests;
