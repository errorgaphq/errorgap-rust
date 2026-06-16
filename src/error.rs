//! Error types for the Errorgap SDK.

use thiserror::Error;

/// Errors returned by the SDK.
#[derive(Debug, Error)]
pub enum ErrorgapError {
    /// `init` has not been called.
    #[error("errorgap: not initialized")]
    NotInitialized,

    /// `project_slug` was missing or blank in the configuration.
    #[error("errorgap: project_slug is required")]
    MissingProjectSlug,

    /// `endpoint` was missing or blank in the configuration.
    #[error("errorgap: endpoint is required")]
    MissingEndpoint,

    /// HTTP delivery failed.
    #[error("errorgap: delivery failed: {0}")]
    Delivery(#[from] reqwest::Error),

    /// JSON encoding failed.
    #[error("errorgap: encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),

    /// The bounded delivery channel was full and the notice was dropped.
    #[error("errorgap: delivery queue full")]
    QueueFull,
}
