//! The Errorgap notice envelope and per-notice options.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::backtrace::{self, Frame};
use crate::config::Configuration;
use crate::filter;
use crate::VERSION;

const NOTIFIER_ID: &str = "errorgap-rust";

/// Per-notice context that augments the configuration defaults.
#[derive(Debug, Default, Clone)]
pub struct NoticeOptions {
    /// Extra fields merged into `context`.
    pub context: Map<String, Value>,
    /// Extra fields merged into `environment`.
    pub environment: Map<String, Value>,
    /// Session fields.
    pub session: Map<String, Value>,
    /// Request/job params (filtered before delivery).
    pub params: Map<String, Value>,
    /// Breadcrumbs attached as `context.breadcrumbs`. The package-level
    /// [`crate::notify`] / [`crate::notify_error`] set this from the global
    /// buffer automatically.
    pub breadcrumbs: Vec<Value>,
}

impl NoticeOptions {
    /// Add a `context` field.
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Add a `params` field.
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
}

/// The wire envelope POSTed to `/api/projects/:slug/notices`.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project_id: Option<String>,
    pub(crate) received_at: String,
    pub(crate) errors: Vec<ErrorEntry>,
    pub(crate) context: Map<String, Value>,
    pub(crate) environment: Map<String, Value>,
    pub(crate) session: Map<String, Value>,
    pub(crate) params: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ErrorEntry {
    pub(crate) r#type: String,
    pub(crate) message: String,
    pub(crate) backtrace: Vec<Frame>,
}

impl Notice {
    pub(crate) fn build<E: std::fmt::Display>(
        error: &E,
        config: &Configuration,
        options: NoticeOptions,
    ) -> Self {
        Self::build_parts(
            short_type_name::<E>(),
            format!("{error}"),
            Vec::new(),
            config,
            options,
        )
    }

    /// Build a notice from a `std::error::Error`, flattening its `source()`
    /// chain into `context.causes`.
    pub(crate) fn build_error<E: std::error::Error>(
        error: &E,
        config: &Configuration,
        options: NoticeOptions,
    ) -> Self {
        Self::build_parts(
            short_type_name::<E>(),
            format!("{error}"),
            collect_causes(error.source()),
            config,
            options,
        )
    }

    fn build_parts(
        type_name: String,
        message: String,
        causes: Vec<Value>,
        config: &Configuration,
        options: NoticeOptions,
    ) -> Self {
        let mut context = Map::new();
        context.insert("notifier".into(), Value::String(NOTIFIER_ID.into()));
        context.insert("notifier_version".into(), Value::String(VERSION.into()));
        context.insert(
            "environment".into(),
            Value::String(config.environment().into()),
        );
        if let Some(release) = config.release() {
            context.insert("release".into(), Value::String(release.into()));
        }
        if !causes.is_empty() {
            context.insert("causes".into(), Value::Array(causes));
        }
        if !options.breadcrumbs.is_empty() {
            context.insert("breadcrumbs".into(), Value::Array(options.breadcrumbs));
        }
        for (k, v) in options.context {
            context.insert(k, v);
        }

        let environment: Map<String, Value> = options.environment;
        let session: Map<String, Value> = options.session;
        let params = filter::filter(&options.params, config.filter_keys());

        let backtrace = backtrace::capture(config.root_directory());
        let error_entry = ErrorEntry {
            r#type: type_name,
            message,
            backtrace,
        };

        Notice {
            project_id: config.project_id().map(String::from),
            received_at: now_rfc3339(),
            errors: vec![error_entry],
            context,
            environment,
            session,
            params,
        }
    }
}

/// Walk a `source()` chain into `[{type, message}]`, nearest cause first.
///
/// Concrete cause types are not recoverable from `&dyn Error`, so `type` is a
/// best-effort label taken from the leading identifier of the `Debug` form.
fn collect_causes(mut source: Option<&(dyn std::error::Error + 'static)>) -> Vec<Value> {
    let mut causes = Vec::new();
    let mut depth = 0;
    while let Some(err) = source {
        if depth >= 10 {
            break;
        }
        let mut entry = Map::new();
        entry.insert("type".into(), Value::String(cause_type(err)));
        entry.insert("message".into(), Value::String(format!("{err}")));
        causes.push(Value::Object(entry));
        source = err.source();
        depth += 1;
    }
    causes
}

fn cause_type(err: &(dyn std::error::Error + 'static)) -> String {
    let debug = format!("{err:?}");
    let ident: String = debug
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        "Error".to_string()
    } else {
        ident
    }
}

fn short_type_name<T: ?Sized>() -> String {
    let full = std::any::type_name::<T>();
    // Drop module path: `my_app::Boom` -> `Boom`.
    full.rsplit("::").next().unwrap_or(full).to_string()
}

/// Format the current UTC time as RFC 3339 without bringing in a date dep.
///
/// `std::time::SystemTime` can't format on its own; this helper formats the
/// Unix timestamp using the same epoch logic the rest of the SDK relies on.
pub(crate) fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    format_rfc3339(secs, nanos)
}

fn format_rfc3339(secs: i64, millis: u32) -> String {
    // Days/months from civil_from_days algorithm by Howard Hinnant.
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    if month <= 2 {
        year += 1;
    }
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_value() {
        // 2024-01-15T12:34:56.000Z corresponds to unix seconds 1705322096.
        assert_eq!(format_rfc3339(1_705_322_096, 0), "2024-01-15T12:34:56.000Z");
    }
}
