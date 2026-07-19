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

mod apm;
mod backtrace;
mod breadcrumbs;
mod client;
mod config;
mod error;
mod filter;
mod logs;
mod notice;

#[cfg(feature = "tower")]
pub mod tower;

#[cfg(feature = "tracing")]
pub mod tracing;

pub use apm::{normalize_sql, Span, SpanCollector, Transaction};
pub use client::{Client, DeliveryResult};
pub use config::{Configuration, ConfigurationBuilder};
pub use error::ErrorgapError;
pub use notice::{Notice, NoticeOptions};

use breadcrumbs::BreadcrumbBuffer;
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use serde_json::{Map, Value};

/// SDK version, embedded in every notice's User-Agent header.
pub const VERSION: &str = "0.2.0";

static DEFAULT_CLIENT: OnceCell<RwLock<Option<Client>>> = OnceCell::new();
static BREADCRUMBS: OnceCell<RwLock<BreadcrumbBuffer>> = OnceCell::new();

fn default_cell() -> &'static RwLock<Option<Client>> {
    DEFAULT_CLIENT.get_or_init(|| RwLock::new(None))
}

fn breadcrumbs_cell() -> &'static RwLock<BreadcrumbBuffer> {
    BREADCRUMBS.get_or_init(|| RwLock::new(BreadcrumbBuffer::new(25)))
}

/// Initialize the package-level default client.
///
/// Subsequent calls replace the existing client; in-flight deliveries on the
/// previous client are dropped (call [`flush`] first if you care).
pub fn init(configuration: Configuration) -> Result<(), ErrorgapError> {
    let max_breadcrumbs = configuration.max_breadcrumbs();
    let client = Client::new(configuration)?;
    *default_cell().write() = Some(client);
    *breadcrumbs_cell().write() = BreadcrumbBuffer::new(max_breadcrumbs);
    Ok(())
}

/// Report an error via the package-level client.
pub fn notify<E: std::fmt::Display>(error: E) -> DeliveryResult {
    notify_with(error, NoticeOptions::default())
}

/// Report an error with additional context.
pub fn notify_with<E: std::fmt::Display>(error: E, mut options: NoticeOptions) -> DeliveryResult {
    inject_breadcrumbs(&mut options);
    let guard = default_cell().read();
    match guard.as_ref() {
        Some(client) => client.notify(error, options),
        None => DeliveryResult::error(ErrorgapError::NotInitialized),
    }
}

/// Report an error that implements [`std::error::Error`], flattening its
/// `source()` chain into `context.causes`.
pub fn notify_error<E: std::error::Error>(error: &E) -> DeliveryResult {
    notify_error_with(error, NoticeOptions::default())
}

/// Report a [`std::error::Error`] with additional context.
pub fn notify_error_with<E: std::error::Error>(
    error: &E,
    mut options: NoticeOptions,
) -> DeliveryResult {
    inject_breadcrumbs(&mut options);
    let guard = default_cell().read();
    match guard.as_ref() {
        Some(client) => client.notify_error(error, options),
        None => DeliveryResult::error(ErrorgapError::NotInitialized),
    }
}

/// Deliver a structured log line via the package-level client.
pub fn log(message: &str, level: &str, source: Option<&str>) -> DeliveryResult {
    let guard = default_cell().read();
    match guard.as_ref() {
        Some(client) => client.notify_log(message, level, source),
        None => DeliveryResult::error(ErrorgapError::NotInitialized),
    }
}

/// Deliver an APM transaction (a web interaction or a background job).
pub fn notify_transaction(transaction: Transaction) -> DeliveryResult {
    let guard = default_cell().read();
    match guard.as_ref() {
        Some(client) => client.notify_transaction(transaction),
        None => DeliveryResult::error(ErrorgapError::NotInitialized),
    }
}

/// Record a diagnostic breadcrumb attached to subsequent notices as
/// `context.breadcrumbs`.
pub fn add_breadcrumb(
    message: impl Into<String>,
    category: Option<&str>,
    metadata: Map<String, Value>,
) {
    let timestamp = notice::now_rfc3339();
    breadcrumbs_cell().write().add(
        message.into(),
        category.map(String::from),
        metadata,
        timestamp,
    );
}

/// Clear all recorded breadcrumbs.
pub fn clear_breadcrumbs() {
    breadcrumbs_cell().write().clear();
}

fn inject_breadcrumbs(options: &mut NoticeOptions) {
    if options.breadcrumbs.is_empty() {
        let crumbs = breadcrumbs_cell().read().snapshot();
        if !crumbs.is_empty() {
            options.breadcrumbs = crumbs;
        }
    }
}

/// Wait for in-flight async deliveries to complete.
pub async fn flush() {
    // Clone the client out so the read guard is released before awaiting.
    let client = default_cell().read().clone();
    if let Some(client) = client {
        client.flush().await;
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
