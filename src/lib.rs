//! Rust notifier for [Errorgap](https://errorgap.com).
//!
//! Use the package-level facade ([`init`], [`notify`], [`flush`]) for the
//! common case, or instantiate a [`Client`] directly for isolated state (e.g.
//! tests, libraries).
//!
//! The [`tower`] middleware layer reports failures from `axum`, `tonic`, and
//! any other tower-based server stack. The [`tracing`] subscriber layer ships
//! `ERROR`-level events as standalone notices.

#![warn(missing_docs)]

mod backtrace;
mod client;
mod config;
mod error;
mod filter;
mod notice;

#[cfg(feature = "tower")]
pub mod tower;

#[cfg(feature = "tracing")]
pub mod tracing;

pub use client::{Client, DeliveryResult};
pub use config::{Configuration, ConfigurationBuilder};
pub use error::ErrorgapError;
pub use notice::{Notice, NoticeOptions};

use once_cell::sync::OnceCell;
use parking_lot::RwLock;

/// SDK version, embedded in every notice's User-Agent header.
pub const VERSION: &str = "0.1.0";

static DEFAULT_CLIENT: OnceCell<RwLock<Option<Client>>> = OnceCell::new();

fn default_cell() -> &'static RwLock<Option<Client>> {
    DEFAULT_CLIENT.get_or_init(|| RwLock::new(None))
}

/// Initialize the package-level default client.
///
/// Subsequent calls replace the existing client; in-flight deliveries on the
/// previous client are dropped (call [`flush`] first if you care).
pub fn init(configuration: Configuration) -> Result<(), ErrorgapError> {
    let client = Client::new(configuration)?;
    *default_cell().write() = Some(client);
    Ok(())
}

/// Report an error via the package-level client.
pub fn notify<E: std::fmt::Display>(error: E) -> DeliveryResult {
    notify_with(error, NoticeOptions::default())
}

/// Report an error with additional context.
pub fn notify_with<E: std::fmt::Display>(error: E, options: NoticeOptions) -> DeliveryResult {
    let guard = default_cell().read();
    match guard.as_ref() {
        Some(client) => client.notify(error, options),
        None => DeliveryResult::error(ErrorgapError::NotInitialized),
    }
}

/// Wait for in-flight async deliveries to complete.
pub async fn flush() {
    let guard = default_cell().read();
    if let Some(client) = guard.as_ref() {
        let fut = client.flush_future();
        drop(guard);
        fut.await;
    }
}

/// Shut down the default client. After this returns, [`notify`] will return
/// [`ErrorgapError::NotInitialized`] until [`init`] is called again.
pub async fn shutdown() {
    let client = default_cell().write().take();
    if let Some(client) = client {
        client.shutdown().await;
    }
}

/// Access the default client, if initialized. Useful for libraries that want
/// to detect whether the host application has set up Errorgap.
pub fn default_client() -> Option<Client> {
    default_cell().read().clone()
}
