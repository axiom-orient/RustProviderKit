//! Typed domain values, pure reducers, and bounded single-consumer streams.
//!
//! This crate owns no filesystem, network, process, browser, or durable storage
//! effects. Those boundaries live in `rust-provider-kit-runtime` and
//! `rust-provider-kit-platform`.

mod account_reducer;
mod accounts;
mod error;
mod event_stream;
mod events;
mod execution_reducer;
mod identifiers;
mod instant;
mod json_value;
mod model;
mod oauth;

pub use account_reducer::*;
pub use accounts::*;
pub use error::*;
pub use event_stream::*;
pub use events::*;
pub use execution_reducer::*;
pub use identifiers::*;
pub use instant::*;
pub use json_value::*;
pub use model::*;
pub use oauth::*;
