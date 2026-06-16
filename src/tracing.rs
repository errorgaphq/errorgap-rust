//! `tracing-subscriber` layer that ships `ERROR` events as Errorgap notices.
//!
//! Enabled by the `tracing` feature (default). Compose with your existing
//! subscriber:
//!
//! ```ignore
//! use tracing_subscriber::prelude::*;
//! tracing_subscriber::registry()
//!     .with(tracing_subscriber::fmt::layer())
//!     .with(errorgap::tracing::ErrorgapLayer)
//!     .init();
//! ```
//!
//! Only `Level::ERROR` events are reported; lower levels are ignored.

use std::fmt::Write as _;

use tracing::Level;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::{notify, NoticeOptions};

/// Subscriber layer that forwards `ERROR` events to Errorgap.
#[derive(Debug, Clone, Default)]
pub struct ErrorgapLayer;

impl<S> Layer<S> for ErrorgapLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = if visitor.message.is_empty() {
            event.metadata().name().to_string()
        } else {
            visitor.message
        };

        let opts = NoticeOptions::default()
            .with_context("source", "tracing")
            .with_context("target", event.metadata().target());
        let _ = notify(StringError(message));
        drop(opts);
    }
}

/// Wraps a string so it can be reported via [`notify`].
struct StringError(String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{:?}", value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        }
    }
}
