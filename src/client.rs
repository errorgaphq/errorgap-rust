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
    /// Deliberately not sent: an ignored environment, or dropped by
    /// `before_notify`. Reported as HTTP 204 with nothing queued.
    pub(crate) fn skipped() -> Self {
        DeliveryResult {
            status: Some(204),
            body: None,
            error: None,
            queued: false,
        }
    }

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
    pause: Arc<Pause>,
    remote: Remote,
}

/// The project's remote settings: what errorgap wants this app to send.
/// Someone can turn errors, performance data or logs off for a project in
/// errorgap (Project settings › What SDKs send) and running apps follow
/// within a poll interval, with no deploy. Until the first answer, and
/// whenever errorgap cannot be reached, everything stays on.
#[derive(Default)]
struct Remote {
    polling: std::sync::atomic::AtomicBool,
    errors_off: std::sync::atomic::AtomicBool,
    apm_off: std::sync::atomic::AtomicBool,
    logs_off: std::sync::atomic::AtomicBool,
    interval: std::sync::atomic::AtomicU64,
}

const DEFAULT_REMOTE_INTERVAL: u64 = 600;
const MIN_REMOTE_INTERVAL: u64 = 60;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(30);
const BACKLOG_SIZE: usize = 100;
const DEFAULT_RETRY_AFTER: u64 = 60;
const MAX_RETRY_AFTER: u64 = 300;

/// Sending is paused until `until` (unix milliseconds) after a 429 or 503.
#[derive(Default)]
struct Pause {
    until: std::sync::atomic::AtomicU64,
    backlog: AtomicUsize,
}

impl Pause {
    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn active(&self) -> bool {
        Self::now() < self.until.load(Ordering::SeqCst)
    }

    fn start(&self, retry_after: Option<&str>) {
        let seconds = retry_after
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETRY_AFTER)
            .clamp(1, MAX_RETRY_AFTER);
        self.until
            .store(Self::now() + seconds * 1000, Ordering::SeqCst);
    }
}

/// Network failures and 5xx responses are worth another try. A 4xx is not:
/// the server understood and refused, and after a 429 the payload is dropped
/// so a limited project is not sent the same burst again.
fn retryable(result: &Result<(u16, String), ErrorgapError>) -> bool {
    match result {
        Ok((status, _)) => *status >= 500,
        Err(ErrorgapError::Delivery(_)) => true,
        Err(_) => false,
    }
}

/// Try again later, off the delivery worker, up to `MAX_ATTEMPTS` in all.
fn retry_later(
    http: reqwest::Client,
    config: Configuration,
    pause: Arc<Pause>,
    resource: Resource,
    body: Value,
) {
    if pause.backlog.load(Ordering::SeqCst) >= BACKLOG_SIZE {
        return;
    }
    pause.backlog.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        for attempt in 1..MAX_ATTEMPTS {
            tokio::time::sleep(RETRY_DELAY * attempt).await;
            let result = deliver_inner(&http, &config, &pause, resource, &body).await;
            if !retryable(&result) {
                break;
            }
        }
        pause.backlog.fetch_sub(1, Ordering::SeqCst);
    });
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
        let pause: Arc<Pause> = Arc::new(Pause::default());

        let tx = if config.is_async() {
            let (tx, mut rx) = mpsc::channel::<(Resource, Value)>(config.queue_size());
            let http_clone = http.clone();
            let cfg_clone = config.clone();
            let pending_clone = pending.clone();
            let pause_clone = pause.clone();
            tokio::spawn(async move {
                while let Some((resource, body)) = rx.recv().await {
                    let result =
                        deliver_inner(&http_clone, &cfg_clone, &pause_clone, resource, &body).await;
                    if retryable(&result) {
                        retry_later(
                            http_clone.clone(),
                            cfg_clone.clone(),
                            pause_clone.clone(),
                            resource,
                            body,
                        );
                    }
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
                pause,
                remote: Remote::default(),
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
        let notice = Notice::build(&error, &self.inner.config, with_transaction(options));
        self.enqueue_notice(notice)
    }

    /// Queue an error that implements [`std::error::Error`], flattening its
    /// `source()` chain into `context.causes`.
    pub fn notify_error<E: std::error::Error>(
        &self,
        error: &E,
        options: NoticeOptions,
    ) -> DeliveryResult {
        let notice = Notice::build_error(error, &self.inner.config, with_transaction(options));
        self.enqueue_notice(notice)
    }

    fn enqueue_notice(&self, notice: Notice) -> DeliveryResult {
        match self.prepare(&notice) {
            Ok(Some(value)) => self.enqueue(Resource::Notices, value),
            Ok(None) => DeliveryResult::skipped(),
            Err(e) => DeliveryResult::error(e),
        }
    }

    /// The notice as JSON after `before_notify`, or `None` when the hook
    /// dropped it or the environment is ignored. A hook that panics is
    /// skipped: a bug in a filter must not lose the error it was filtering.
    fn prepare(&self, notice: &Notice) -> Result<Option<Value>, ErrorgapError> {
        if self.inner.config.ignored_environment() {
            return Ok(None);
        }
        let mut value = serde_json::to_value(notice).map_err(ErrorgapError::Encoding)?;
        if let Some(hook) = self.inner.config.before_notify() {
            let mut candidate = value.clone();
            let kept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let keep = (hook.0)(&mut candidate);
                (keep, candidate)
            }));
            match kept {
                Ok((false, _)) => return Ok(None),
                Ok((true, changed)) => value = changed,
                Err(_) => {}
            }
        }
        Ok(Some(value))
    }

    /// False only when errorgap has said to stop sending this kind of
    /// payload. The first call starts the background poll (when a Tokio
    /// runtime is running).
    fn remote_enabled(&self, resource: Resource) -> bool {
        if !self.inner.config.remote_settings() {
            return true;
        }
        let remote = &self.inner.remote;
        if !remote.polling.swap(true, Ordering::SeqCst) {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // Weak, so the poll ends with the client instead of keeping
                // it (and its delivery channel) alive.
                let weak = Arc::downgrade(&self.inner);
                handle.spawn(async move {
                    loop {
                        let Some(inner) = weak.upgrade() else { break };
                        let client = Client { inner };
                        client.refresh_remote_settings().await;
                        let seconds = client.inner.remote.interval.load(Ordering::SeqCst);
                        drop(client);
                        let seconds = if seconds >= MIN_REMOTE_INTERVAL {
                            seconds
                        } else {
                            DEFAULT_REMOTE_INTERVAL
                        };
                        tokio::time::sleep(Duration::from_secs(seconds)).await;
                    }
                });
            } else {
                remote.polling.store(false, Ordering::SeqCst);
            }
        }
        let off = match resource {
            Resource::Notices => &remote.errors_off,
            Resource::Transactions => &remote.apm_off,
            Resource::Logs => &remote.logs_off,
        };
        !off.load(Ordering::SeqCst)
    }

    /// Fetch the project's remote settings now. Returns whether they could
    /// be read; a failure leaves the last known settings in place.
    pub async fn refresh_remote_settings(&self) -> bool {
        let config = &self.inner.config;
        let url = format!(
            "{}/api/projects/{}/config",
            config.endpoint().trim_end_matches('/'),
            config.project_slug()
        );
        let mut request = self.inner.http.get(url);
        if let Some(api_key) = config.api_key().filter(|key| !key.is_empty()) {
            request = request.header("x-errorgap-project-key", api_key);
        }
        let Ok(response) = request.send().await else {
            return false;
        };
        if response.status().as_u16() != 200 {
            return false;
        }
        let Ok(body) = response.json::<Value>().await else {
            return false;
        };
        if !body.is_object() {
            return false;
        }
        // An absent field means on.
        let off = |field: &str| body.get(field).and_then(Value::as_bool) == Some(false);
        let remote = &self.inner.remote;
        remote
            .errors_off
            .store(off("errors_enabled"), Ordering::SeqCst);
        remote.apm_off.store(off("apm_enabled"), Ordering::SeqCst);
        remote.logs_off.store(off("logs_enabled"), Ordering::SeqCst);
        remote.interval.store(
            body.get("poll_interval_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            Ordering::SeqCst,
        );
        true
    }

    /// True while delivery is paused at the server's request (a 429 or 503
    /// with `Retry-After`).
    pub fn paused(&self) -> bool {
        self.inner.pause.active()
    }

    /// Deliver synchronously from within an async context. Use this in tests
    /// or when the caller wants to inspect the HTTP status of a specific
    /// notice. The async worker is not involved.
    pub async fn notify_sync<E: std::fmt::Display>(
        &self,
        error: E,
        options: NoticeOptions,
    ) -> DeliveryResult {
        let notice = Notice::build(&error, &self.inner.config, with_transaction(options));
        match self.prepare(&notice) {
            Ok(Some(value)) => self.deliver_value_sync(Resource::Notices, value).await,
            Ok(None) => DeliveryResult::skipped(),
            Err(e) => DeliveryResult::error(e),
        }
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
        if self.inner.config.ignored_environment() || !self.remote_enabled(resource) {
            return DeliveryResult::skipped();
        }
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

    async fn deliver_value_sync(&self, resource: Resource, value: Value) -> DeliveryResult {
        if self.inner.config.ignored_environment() || !self.remote_enabled(resource) {
            return DeliveryResult::skipped();
        }
        match deliver_inner(
            &self.inner.http,
            &self.inner.config,
            &self.inner.pause,
            resource,
            &value,
        )
        .await
        {
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
    pause: &Pause,
    resource: Resource,
    body: &Value,
) -> Result<(u16, String), ErrorgapError> {
    if pause.active() {
        return Err(ErrorgapError::RateLimited);
    }
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
    if status == 429 || status == 503 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok());
        pause.start(retry_after);
    }
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

/// Attach the current transaction's id (see [`crate::in_transaction`]) unless
/// the caller set one, so errorgap links the error to its request.
fn with_transaction(mut options: NoticeOptions) -> NoticeOptions {
    if !options.context.contains_key("transaction_id") {
        if let Some(id) = crate::apm::current_transaction_id() {
            options.context.insert("transaction_id".into(), id.into());
        }
    }
    options
}
