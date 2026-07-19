//! The HTTP client and async delivery worker.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::apm::Transaction;
use crate::config::Configuration;
use crate::error::ErrorgapError;
use crate::logs;
use crate::notice::{now_rfc3339, Notice, NoticeOptions};
use crate::VERSION;

/// Ingestion resource a queued payload is delivered to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Resource {
    Notices,
    Transactions,
    Logs,
}

impl Resource {
    fn path(self) -> &'static str {
        match self {
            Resource::Notices => "notices",
            Resource::Transactions => "transactions",
            Resource::Logs => "logs",
        }
    }
}

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
    tx: Option<mpsc::Sender<(Resource, Value)>>,
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
            let (tx, mut rx) = mpsc::channel::<(Resource, Value)>(config.queue_size());
            let http_clone = http.clone();
            let cfg_clone = config.clone();
            let pending_clone = pending.clone();
            tokio::spawn(async move {
                while let Some((resource, body)) = rx.recv().await {
                    let _ = deliver_inner(&http_clone, &cfg_clone, resource, &body).await;
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
        self.enqueue_notice(notice)
    }

    /// Queue an error that implements [`std::error::Error`], flattening its
    /// `source()` chain into `context.causes`.
    pub fn notify_error<E: std::error::Error>(
        &self,
        error: &E,
        options: NoticeOptions,
    ) -> DeliveryResult {
        let notice = Notice::build_error(error, &self.inner.config, options);
        self.enqueue_notice(notice)
    }

    fn enqueue_notice(&self, notice: Notice) -> DeliveryResult {
        match serde_json::to_value(&notice) {
            Ok(value) => self.enqueue(Resource::Notices, value),
            Err(e) => DeliveryResult::error(ErrorgapError::Encoding(e)),
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
        self.deliver_sync(Resource::Notices, &notice).await
    }

    /// Queue an APM transaction (a web interaction or a background job).
    pub fn notify_transaction(&self, transaction: Transaction) -> DeliveryResult {
        if !self.inner.config.apm_enabled() || !sample(self.inner.config.apm_sample_rate()) {
            return DeliveryResult {
                status: Some(204),
                body: None,
                error: None,
                queued: false,
            };
        }
        let value = transaction.payload(self.inner.config.environment(), now_rfc3339);
        self.enqueue(Resource::Transactions, value)
    }

    /// Deliver an APM transaction synchronously.
    pub async fn notify_transaction_sync(&self, transaction: Transaction) -> DeliveryResult {
        if !self.inner.config.apm_enabled() || !sample(self.inner.config.apm_sample_rate()) {
            return DeliveryResult {
                status: Some(204),
                body: None,
                error: None,
                queued: false,
            };
        }
        let value = transaction.payload(self.inner.config.environment(), now_rfc3339);
        self.deliver_value_sync(Resource::Transactions, value).await
    }

    /// Queue a structured log line.
    pub fn notify_log(&self, message: &str, level: &str, source: Option<&str>) -> DeliveryResult {
        match self.build_log(message, level, source) {
            LogOutcome::Below => DeliveryResult {
                status: Some(204),
                body: None,
                error: None,
                queued: false,
            },
            LogOutcome::Payload(value) => self.enqueue(Resource::Logs, value),
        }
    }

    /// Deliver a structured log line synchronously.
    pub async fn notify_log_sync(
        &self,
        message: &str,
        level: &str,
        source: Option<&str>,
    ) -> DeliveryResult {
        match self.build_log(message, level, source) {
            LogOutcome::Below => DeliveryResult {
                status: Some(204),
                body: None,
                error: None,
                queued: false,
            },
            LogOutcome::Payload(value) => self.deliver_value_sync(Resource::Logs, value).await,
        }
    }

    fn build_log(&self, message: &str, level: &str, source: Option<&str>) -> LogOutcome {
        let normalized = logs::normalize_level(level);
        let threshold =
            logs::level_rank(logs::normalize_level(self.inner.config.minimum_log_level()));
        if !self.inner.config.logs_enabled() || logs::level_rank(normalized) < threshold {
            return LogOutcome::Below;
        }
        let mut map = serde_json::Map::new();
        map.insert("message".into(), Value::String(message.to_string()));
        map.insert("level".into(), Value::String(normalized.to_string()));
        map.insert(
            "environment".into(),
            Value::String(self.inner.config.environment().to_string()),
        );
        map.insert("occurred_at".into(), Value::String(now_rfc3339()));
        if let Some(source) = source.filter(|s| !s.is_empty()) {
            map.insert("source".into(), Value::String(source.to_string()));
        }
        LogOutcome::Payload(Value::Object(map))
    }

    fn enqueue(&self, resource: Resource, value: Value) -> DeliveryResult {
        let Some(tx) = &self.inner.tx else {
            // Sync mode without an explicit await — direct callers to *_sync.
            return DeliveryResult::error(ErrorgapError::NotInitialized);
        };
        // Increment before send so flush won't race past an item that
        // hasn't been pulled off the channel yet.
        self.inner.pending.fetch_add(1, Ordering::SeqCst);
        match tx.try_send((resource, value)) {
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

    async fn deliver_sync(&self, resource: Resource, notice: &Notice) -> DeliveryResult {
        match serde_json::to_value(notice) {
            Ok(value) => self.deliver_value_sync(resource, value).await,
            Err(e) => DeliveryResult::error(ErrorgapError::Encoding(e)),
        }
    }

    async fn deliver_value_sync(&self, resource: Resource, value: Value) -> DeliveryResult {
        match deliver_inner(&self.inner.http, &self.inner.config, resource, &value).await {
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

enum LogOutcome {
    Below,
    Payload(Value),
}

/// Best-effort sampling decision without pulling in an RNG dependency.
fn sample(rate: f64) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64 / 1_000_000_000.0) < rate
}

async fn deliver_inner(
    http: &reqwest::Client,
    config: &Configuration,
    resource: Resource,
    body: &Value,
) -> Result<(u16, String), ErrorgapError> {
    let url = format!(
        "{}/api/projects/{}/{}",
        config.endpoint().trim_end_matches('/'),
        config.project_slug(),
        resource.path()
    );

    let mut req = http
        .post(url)
        .header("content-type", "application/json")
        .json(body);

    if let Some(api_key) = config.api_key() {
        if !api_key.is_empty() {
            let name = HeaderName::from_static("x-errorgap-project-key");
            let value =
                HeaderValue::from_str(api_key).map_err(|_| ErrorgapError::MissingEndpoint)?; // reuse error for invalid header
            req = req.header(name, value);
        }
    }

    let response = req.send().await.map_err(ErrorgapError::Delivery)?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}
