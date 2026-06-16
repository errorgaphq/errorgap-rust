# errorgap (Rust)

Rust notifier for [Errorgap](https://errorgap.com). Async-by-default
delivery via `reqwest`, with optional tower middleware for axum / tonic
and a `tracing-subscriber` layer that ships `ERROR` events.

Requires Rust 1.75+.

## Install

```toml
[dependencies]
errorgap = "0.1"
```

Or, to skip the tower / tracing integrations:

```toml
errorgap = { version = "0.1", default-features = false }
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

## Configuration reference

| Field | Default | Notes |
|---|---|---|
| `endpoint` | `ERRORGAP_ENDPOINT` or `http://127.0.0.1:3030` | |
| `project_slug` | `ERRORGAP_PROJECT_SLUG` | **Required** |
| `project_id` | `ERRORGAP_PROJECT_ID` | |
| `api_key` | `ERRORGAP_API_KEY` | Sent as `x-errorgap-project-key` |
| `environment` | `ERRORGAP_ENVIRONMENT` or `"production"` | |
| `release` | — | |
| `is_async` | `true` | Bounded `mpsc` channel + worker task |
| `filter_keys` | `["password", "token", ...]` | Substring, case-insensitive |
| `timeout` | `5 s` | HTTP request timeout |
| `queue_size` | `100` | Bounded async queue |

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
