//! Configuration for the Errorgap SDK.

use std::time::Duration;

use crate::ErrorgapError;

/// Default keys (case-insensitive substrings) used to mask sensitive values
/// before they reach the wire.
pub const DEFAULT_FILTER_KEYS: &[&str] = &[
    "password",
    "password_confirmation",
    "token",
    "secret",
    "api_key",
    "authorization",
    "cookie",
];

/// SDK configuration.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub(crate) endpoint: String,
    pub(crate) project_slug: String,
    pub(crate) project_id: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) environment: String,
    pub(crate) release: Option<String>,
    pub(crate) is_async: bool,
    pub(crate) filter_keys: Vec<String>,
    pub(crate) timeout: Duration,
    pub(crate) queue_size: usize,
}

impl Configuration {
    /// Start a new builder.
    pub fn builder() -> ConfigurationBuilder {
        ConfigurationBuilder::new()
    }

    /// Build from environment variables (`ERRORGAP_*`).
    pub fn from_env() -> Result<Self, ErrorgapError> {
        ConfigurationBuilder::new().from_env().build()
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn project_slug(&self) -> &str {
        &self.project_slug
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub(crate) fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    pub(crate) fn environment(&self) -> &str {
        &self.environment
    }

    pub(crate) fn release(&self) -> Option<&str> {
        self.release.as_deref()
    }

    pub(crate) fn filter_keys(&self) -> &[String] {
        &self.filter_keys
    }

    pub(crate) fn is_async(&self) -> bool {
        self.is_async
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn queue_size(&self) -> usize {
        self.queue_size
    }
}

/// Builder for [`Configuration`].
#[derive(Debug, Default, Clone)]
pub struct ConfigurationBuilder {
    endpoint: Option<String>,
    project_slug: Option<String>,
    project_id: Option<String>,
    api_key: Option<String>,
    environment: Option<String>,
    release: Option<String>,
    is_async: Option<bool>,
    filter_keys: Option<Vec<String>>,
    timeout: Option<Duration>,
    queue_size: Option<usize>,
}

impl ConfigurationBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read `ERRORGAP_ENDPOINT`, `ERRORGAP_PROJECT_SLUG`,
    /// `ERRORGAP_PROJECT_ID`, `ERRORGAP_API_KEY`, `ERRORGAP_ENVIRONMENT`
    /// from the process environment for any field not already set.
    pub fn from_env(mut self) -> Self {
        self.endpoint = self.endpoint.or_else(|| std::env::var("ERRORGAP_ENDPOINT").ok());
        self.project_slug = self.project_slug.or_else(|| std::env::var("ERRORGAP_PROJECT_SLUG").ok());
        self.project_id = self.project_id.or_else(|| std::env::var("ERRORGAP_PROJECT_ID").ok());
        self.api_key = self.api_key.or_else(|| std::env::var("ERRORGAP_API_KEY").ok());
        self.environment = self.environment.or_else(|| std::env::var("ERRORGAP_ENVIRONMENT").ok());
        self
    }

    /// Errorgap endpoint base URL.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Errorgap project slug (required).
    pub fn project_slug(mut self, slug: impl Into<String>) -> Self {
        self.project_slug = Some(slug.into());
        self
    }

    /// Optional Errorgap project id (embedded in notice payload).
    pub fn project_id(mut self, id: impl Into<String>) -> Self {
        self.project_id = Some(id.into());
        self
    }

    /// Errorgap API key, sent as `x-errorgap-project-key`.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Deployment environment label.
    pub fn environment(mut self, env: impl Into<String>) -> Self {
        self.environment = Some(env.into());
        self
    }

    /// App release/version identifier.
    pub fn release(mut self, release: impl Into<String>) -> Self {
        self.release = Some(release.into());
        self
    }

    /// Set whether delivery is async (background channel) or sync.
    pub fn is_async(mut self, is_async: bool) -> Self {
        self.is_async = Some(is_async);
        self
    }

    /// Override the default filter-key substring list.
    pub fn filter_keys(mut self, keys: Vec<String>) -> Self {
        self.filter_keys = Some(keys);
        self
    }

    /// Override the HTTP request timeout (default: 5s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Override the bounded async-delivery channel size (default: 100).
    pub fn queue_size(mut self, size: usize) -> Self {
        self.queue_size = Some(size);
        self
    }

    /// Build the configuration, validating required fields.
    pub fn build(self) -> Result<Configuration, ErrorgapError> {
        let endpoint = self
            .endpoint
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3030".to_string());
        let project_slug = self
            .project_slug
            .filter(|s| !s.trim().is_empty())
            .ok_or(ErrorgapError::MissingProjectSlug)?;
        if endpoint.trim().is_empty() {
            return Err(ErrorgapError::MissingEndpoint);
        }

        Ok(Configuration {
            endpoint,
            project_slug,
            project_id: self.project_id,
            api_key: self.api_key,
            environment: self.environment.unwrap_or_else(|| "production".to_string()),
            release: self.release,
            is_async: self.is_async.unwrap_or(true),
            filter_keys: self
                .filter_keys
                .unwrap_or_else(|| DEFAULT_FILTER_KEYS.iter().map(|s| s.to_string()).collect()),
            timeout: self.timeout.unwrap_or(Duration::from_secs(5)),
            queue_size: self.queue_size.unwrap_or(100),
        })
    }
}
