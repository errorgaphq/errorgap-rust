//! The HTTP client and async delivery worker.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::Configuration;
use crate::error::ErrorgapError;
use crate::notice::{Notice, NoticeOptions};
use crate::VERSION;

/// Outcome of a single delivery attempt.
#[derive(Debug)]
pub struct DeliveryResult {
    /// HTTP status when delivery completed.
    pub status: Option<u16>,
    /// Raw response body, if any.
    pub body: Option<String>,
    /// Failure reason, when delivery did not complete.
    pub error: Option<ErrorgapError>,
    /// `true` when the notice was queued for async delivery and the HTTP
    /// status will be observable only after `flush`.
    pub queued: bool,
}

impl DeliveryResult {
    pub(crate) fn error(error: ErrorgapError) -> Self {
        DeliveryResult {
            status: None,
            body: None,
            error: Some(error),
            queued: false,
        }
    }

    /// `true` when the server returned 2xx.
    pub fn success(&self) -> bool {
        self.error.is_none() && matches!(self.status, Some(s) if (200..300).contains(&s))
    }
}

/// Async-by-default Errorgap delivery client.
///
/// Cloning is cheap (internal `Arc`). All clones share the same async worker
/// and configuration.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    config: Configuration,
    http: reqwest::Client,
    tx: Option<mpsc::Sender<Value>>,
    pending: Arc<AtomicUsize>,
}

impl Client {
    /// Build a new client with the given configuration.
    pub fn new(config: Configuration) -> Result<Self, ErrorgapError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout())
            .user_agent(format!("errorgap-rust/{VERSION}"))
            .build()
            .map_err(ErrorgapError::Delivery)?;

        let pending: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

        let tx = if config.is_async() {
            let (tx, mut rx) = mpsc::channel::<Value>(config.queue_size());
            let http_clone = http.clone();
            let cfg_clone = config.clone();
            let pending_clone = pending.clone();
            tokio::spawn(async move {
                while let Some(notice) = rx.recv().await {
                    let _ = deliver_inner(&http_clone, &cfg_clone, &notice).await;
                    pending_clone.fetch_sub(1, Ordering::SeqCst);
                }
            });
            Some(tx)
        } else {
            None
        };

        Ok(Client {
            inner: Arc::new(Inner {
                config,
                http,
                tx,
                pending,
            }),
        })
    }

    /// Borrow the client's configuration.
    pub fn config(&self) -> &Configuration {
        &self.inner.config
    }

    /// Queue an error for delivery. Returns immediately with `queued = true`;
    /// call [`flush`](Self::flush) (or the package-level [`crate::flush`])
    /// to wait for delivery to complete.
    ///
    /// In sync mode (`is_async = false`) the queue is unused — call
    /// [`notify_sync`](Self::notify_sync) from an async context instead.
    pub fn notify<E: std::fmt::Display>(&self, error: E, options: NoticeOptions) -> DeliveryResult {
        let notice = Notice::build(&error, &self.inner.config, options);
        let value = match serde_json::to_value(&notice) {
            Ok(v) => v,
            Err(e) => return DeliveryResult::error(ErrorgapError::Encoding(e)),
        };

        let Some(tx) = &self.inner.tx else {
            // Sync mode without an explicit await — direct callers to notify_sync.
            return DeliveryResult::error(ErrorgapError::NotInitialized);
        };

        // Increment before send so flush won't race past a notice that
        // hasn't been pulled off the channel yet.
        self.inner.pending.fetch_add(1, Ordering::SeqCst);
        match tx.try_send(value) {
            Ok(()) => DeliveryResult {
                status: Some(202),
                body: None,
                error: None,
                queued: true,
            },
            Err(_) => {
                self.inner.pending.fetch_sub(1, Ordering::SeqCst);
                DeliveryResult::error(ErrorgapError::QueueFull)
            }
        }
    }

    /// Deliver synchronously from within an async context. Use this in tests
    /// or when the caller wants to inspect the HTTP status of a specific
    /// notice. The async worker is not involved.
    pub async fn notify_sync<E: std::fmt::Display>(
        &self,
        error: E,
        options: NoticeOptions,
    ) -> DeliveryResult {
        let notice = Notice::build(&error, &self.inner.config, options);
        let value = match serde_json::to_value(&notice) {
            Ok(v) => v,
            Err(e) => return DeliveryResult::error(ErrorgapError::Encoding(e)),
        };
        match deliver_inner(&self.inner.http, &self.inner.config, &value).await {
            Ok((status, body)) => DeliveryResult {
                status: Some(status),
                body: Some(body),
                error: None,
                queued: false,
            },
            Err(e) => DeliveryResult::error(e),
        }
    }

    /// Wait for all queued async deliveries to complete.
    pub async fn flush(&self) {
        self.flush_future().await;
    }

    pub(crate) fn flush_future(&self) -> impl std::future::Future<Output = ()> {
        let pending = self.inner.pending.clone();
        async move {
            while pending.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    /// Drop the channel; in-flight deliveries finish, then the worker exits.
    pub async fn shutdown(self) {
        let pending = self.inner.pending.clone();
        // Wait for queued deliveries to drain. Other clones may keep the
        // channel alive — they share the same pending counter.
        while pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn deliver_inner(
    http: &reqwest::Client,
    config: &Configuration,
    body: &Value,
) -> Result<(u16, String), ErrorgapError> {
    let url = format!(
        "{}/api/projects/{}/notices",
        config.endpoint().trim_end_matches('/'),
        config.project_slug()
    );

    let mut req = http
        .post(url)
        .header("content-type", "application/json")
        .json(body);

    if let Some(api_key) = config.api_key() {
        if !api_key.is_empty() {
            let name = HeaderName::from_static("x-errorgap-project-key");
            let value = HeaderValue::from_str(api_key)
                .map_err(|_| ErrorgapError::MissingEndpoint)?; // reuse error for invalid header
            req = req.header(name, value);
        }
    }

    let response = req.send().await.map_err(ErrorgapError::Delivery)?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}
