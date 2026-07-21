# errorgap (Rust)

Rust notifier for [Errorgap](https://errorgap.com). Async-by-default
delivery via `reqwest`, with source-aware backtraces, nested `source()`
cause chains, breadcrumbs, structured logs, and APM transactions — plus
optional tower middleware for axum / tonic and a `tracing-subscriber` layer
that ships `ERROR` events.

Backtrace frames are resolved against the local source tree (file, line,
function, an app-versus-vendor flag, and a surrounding source excerpt) so the
dashboard renders highlighted source without any repository integration.

Requires Rust 1.75+.

## Install

```toml
[dependencies]
errorgap = "0.2"
```

Or, to skip the tower / tracing integrations:

```toml
errorgap = { version = "0.2", default-features = false }
```

## Configure

```rust
use errorgap::Configuration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    errorgap::init(
        Configuration::builder()
            .endpoint(std::env::var("ERRORGAP_ENDPOINT")?)
            .project_slug(std::env::var("ERRORGAP_PROJECT_SLUG")?)
            .api_key(std::env::var("ERRORGAP_API_KEY")?)
            .environment(std::env::var("APP_ENV").unwrap_or_else(|_| "production".into()))
            .build()?,
    )?;

    run_app().await?;

    errorgap::flush().await;
    errorgap::shutdown().await;
    Ok(())
}
```

`Configuration::from_env()` is a shorthand that reads the same `ERRORGAP_*`
variables.

## Manual notification

```rust
if let Err(err) = risky().await {
    errorgap::notify(&err);
    return Err(err.into());
}
```

`notify` never panics. Returns a `DeliveryResult` (`status`, `body`,
`error`, `queued`).

## Axum / Tower

```rust
use tower::ServiceBuilder;

let app = axum::Router::new()
    .route("/", axum::routing::get(handler))
    .layer(ServiceBuilder::new().layer(errorgap::tower::ErrorgapLayer::new()));
```

The layer reports any non-`Infallible` error returned by the inner service.

## Tracing

```rust
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(errorgap::tracing::ErrorgapLayer::default())
    .init();
```

Only `Level::ERROR` events are reported.

## Cause chains

`notify_error` accepts any `std::error::Error` and flattens its `source()`
chain into `context.causes`:

```rust
if let Err(err) = checkout().await {
    errorgap::notify_error(&err); // records each source() cause
}
```

## Breadcrumbs

```rust
use serde_json::Map;
errorgap::add_breadcrumb("loaded checkout", Some("navigation"), Map::new());
```

Recorded breadcrumbs are attached to every subsequent notice as
`context.breadcrumbs`.

## Structured logs

```rust
errorgap::log("payment gateway timeout", "error", Some("payments"));
```

Levels are `trace < debug < info < warn < error < fatal`; anything below
`minimum_log_level` is dropped client-side.

## Performance (APM)

Build a transaction, attach spans, and deliver it:

```rust
use errorgap::{SpanCollector, Transaction};

let mut spans = SpanCollector::new();
spans.database("SELECT * FROM orders WHERE id = 123", 4.2);
spans.external(88.0);

errorgap::notify_transaction(
    Transaction::web("GET", "/orders/{id}", "/orders/123")
        .status_code(200)
        .duration_ms(120.0)
        .spans(spans),
);

// Background work:
errorgap::notify_transaction(
    Transaction::job("ReceiptJob", "mailers").duration_ms(40.0),
);
```

`path` is the normalized route template used for grouping; `path_raw` is the
concrete URL. APM delivery requires `apm_enabled` (default `true`).

## Configuration reference

| Field | Default | Notes |
|---|---|---|
| `endpoint` | `ERRORGAP_ENDPOINT` or `http://127.0.0.1:3030` | |
| `project_slug` | `ERRORGAP_PROJECT_SLUG` | **Required** |
| `project_id` | `ERRORGAP_PROJECT_ID` | |
| `api_key` | `ERRORGAP_API_KEY` | Sent as `x-errorgap-project-key` |
| `environment` | `ERRORGAP_ENVIRONMENT` or `"production"` | |
| `release` | — | |
| `root_directory` | `CARGO_MANIFEST_DIR` then cwd | Resolves backtrace source files |
| `is_async` | `true` | Bounded `mpsc` channel + worker task |
| `filter_keys` | `["password", "token", ...]` | Substring, case-insensitive |
| `timeout` | `5 s` | HTTP request timeout |
| `queue_size` | `100` | Bounded async queue |
| `apm_enabled` | `true` | Deliver APM transactions |
| `apm_sample_rate` | `1.0` | Fraction (0..=1) of transactions delivered |
| `logs_enabled` | `true` | Deliver structured logs |
| `minimum_log_level` | `"info"` | Drop logs below this level |
| `max_breadcrumbs` | `25` | Breadcrumbs retained per notice |

## Graceful shutdown

```rust
errorgap::flush().await;
errorgap::shutdown().await;
```

## Backtraces

The SDK uses `std::backtrace::Backtrace::capture`. Set
`RUST_BACKTRACE=1` (or `RUST_BACKTRACE=full`) in your runtime
environment to get frame data; without it, the SDK reports
empty backtraces and the server's grouping falls back to error type +
message.

## Development

```sh
cargo test
```

## License

MIT.
