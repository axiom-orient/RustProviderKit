//! Platform effects for RustProviderKit.
//!
//! This crate provides RFC 7636 PKCE generation and a bounded loopback OAuth
//! authorization session. Provider routing and model execution remain in
//! `rust-provider-kit-runtime`.

mod loopback;
mod loopback_callback;
mod pkce;

pub use loopback::{LoopbackAuthorizationSession, PreparedLoopbackAuthorization};
pub use pkce::ProviderPkceGenerator;

#[cfg(test)]
mod tests;
