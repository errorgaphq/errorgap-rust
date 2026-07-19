//! APM transactions and spans.

use serde::Serialize;
use serde_json::{Map, Value};

/// One APM span (a database query or outbound HTTP call) recorded while a
/// transaction or job is in flight.
#[derive(Debug, Clone, Serialize)]
pub struct Span {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sql: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(rename = "fn_name", skip_serializing_if = "Option::is_none")]
    function: Option<String>,
    duration_ms: f64,
}

impl Span {
    /// A database query span. The SQL is normalized so query shapes aggregate.
    pub fn database(sql: impl AsRef<str>, duration_ms: f64) -> Self {
        Span {
            kind: "db".into(),
            sql: Some(normalize_sql(sql.as_ref())),
            file: None,
            line: None,
            function: None,
            duration_ms,
        }
    }

    /// An outbound HTTP / external service span.
    pub fn external(duration_ms: f64) -> Self {
        Span {
            kind: "http".into(),
            sql: None,
            file: None,
            line: None,
            function: None,
            duration_ms,
        }
    }

    /// Attach a source location (file, line, function) to the span.
    pub fn at(mut self, file: impl Into<String>, line: u32, function: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self.line = Some(line);
        self.function = Some(function.into());
        self
    }
}

/// Collects spans recorded while a transaction or job is in flight.
#[derive(Debug, Default, Clone)]
pub struct SpanCollector {
    spans: Vec<Span>,
}

impl SpanCollector {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a pre-built span.
    pub fn add(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// Record a database query span.
    pub fn database(&mut self, sql: impl AsRef<str>, duration_ms: f64) {
        self.spans.push(Span::database(sql, duration_ms));
    }

    /// Record an outbound HTTP span.
    pub fn external(&mut self, duration_ms: f64) {
        self.spans.push(Span::external(duration_ms));
    }

    pub(crate) fn into_spans(self) -> Vec<Span> {
        self.spans
    }
}

/// An APM transaction: a web interaction (`kind = "web"`) or a background job
/// (`kind = "job"`).
#[derive(Debug, Clone)]
pub struct Transaction {
    kind: String,
    method: Option<String>,
    path: Option<String>,
    path_raw: Option<String>,
    status_code: Option<i32>,
    duration_ms: f64,
    environment: Option<String>,
    occurred_at: Option<String>,
    spans: Vec<Span>,
    job_class: Option<String>,
    queue: Option<String>,
}

impl Transaction {
    /// A web transaction for the given normalized route template
    /// (`path`, e.g. `/orders/{id}`) and concrete `path_raw`.
    pub fn web(
        method: impl Into<String>,
        path: impl Into<String>,
        path_raw: impl Into<String>,
    ) -> Self {
        Transaction {
            kind: "web".into(),
            method: Some(method.into()),
            path: Some(path.into()),
            path_raw: Some(path_raw.into()),
            status_code: None,
            duration_ms: 0.0,
            environment: None,
            occurred_at: None,
            spans: Vec::new(),
            job_class: None,
            queue: None,
        }
    }

    /// A background-job transaction for the given job class and queue.
    pub fn job(job_class: impl Into<String>, queue: impl Into<String>) -> Self {
        Transaction {
            kind: "job".into(),
            method: None,
            path: None,
            path_raw: None,
            status_code: None,
            duration_ms: 0.0,
            environment: None,
            occurred_at: None,
            spans: Vec::new(),
            job_class: Some(job_class.into()),
            queue: Some(queue.into()),
        }
    }

    /// Set the HTTP status code.
    pub fn status_code(mut self, status: i32) -> Self {
        self.status_code = Some(status);
        self
    }

    /// Set the total duration in milliseconds.
    pub fn duration_ms(mut self, duration_ms: f64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Override the environment (defaults to the client's configured value).
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// Override the occurrence timestamp (RFC 3339; defaults to now).
    pub fn occurred_at(mut self, occurred_at: impl Into<String>) -> Self {
        self.occurred_at = Some(occurred_at.into());
        self
    }

    /// Attach the spans recorded during the transaction.
    pub fn spans(mut self, collector: SpanCollector) -> Self {
        self.spans = collector.into_spans();
        self
    }

    pub(crate) fn payload(&self, default_environment: &str, now: impl Fn() -> String) -> Value {
        let mut map = Map::new();
        map.insert("kind".into(), Value::String(self.kind.clone()));
        map.insert(
            "duration_ms".into(),
            serde_json::Number::from_f64(self.duration_ms)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "environment".into(),
            Value::String(
                self.environment
                    .clone()
                    .unwrap_or_else(|| default_environment.to_string()),
            ),
        );
        map.insert(
            "occurred_at".into(),
            Value::String(self.occurred_at.clone().unwrap_or_else(now)),
        );
        map.insert(
            "spans".into(),
            serde_json::to_value(&self.spans).unwrap_or(Value::Array(Vec::new())),
        );
        if let Some(v) = &self.method {
            map.insert("method".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.path {
            map.insert("path".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.path_raw {
            map.insert("path_raw".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.status_code {
            map.insert("status_code".into(), Value::Number(v.into()));
        }
        if let Some(v) = &self.job_class {
            map.insert("job_class".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.queue {
            map.insert("queue".into(), Value::String(v.clone()));
        }
        Value::Object(map)
    }
}

/// Strip literals so query shapes aggregate: `'…'` and numbers become `?`.
pub fn normalize_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            // Consume a single-quoted string literal (handles '' escapes).
            out.push('?');
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        continue;
                    }
                    break;
                }
            }
        } else if c.is_ascii_digit() {
            out.push('?');
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() || n == '.' {
                    chars.next();
                } else {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    // Collapse whitespace runs.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_sql_literals() {
        assert_eq!(
            normalize_sql("SELECT * FROM orders WHERE id = 42 AND name = 'alice'"),
            "SELECT * FROM orders WHERE id = ? AND name = ?"
        );
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(normalize_sql("SELECT\n  1\n  FROM   t"), "SELECT ? FROM t");
    }

    #[test]
    fn database_span_shape() {
        let span =
            Span::database("SELECT * FROM t WHERE id = 7", 12.5).at("repo.rs", 20, "Repo::load");
        let value = serde_json::to_value(&span).unwrap();
        assert_eq!(value["kind"], "db");
        assert_eq!(value["sql"], "SELECT * FROM t WHERE id = ?");
        assert_eq!(value["fn_name"], "Repo::load");
        assert_eq!(value["duration_ms"], 12.5);
    }

    #[test]
    fn web_transaction_payload() {
        let mut collector = SpanCollector::new();
        collector.database("SELECT 1", 3.0);
        collector.external(50.0);
        let txn = Transaction::web("POST", "/orders/{id}", "/orders/7")
            .status_code(201)
            .duration_ms(120.0)
            .spans(collector);
        let value = txn.payload("production", || "2026-01-01T00:00:00.000Z".to_string());
        assert_eq!(value["kind"], "web");
        assert_eq!(value["path"], "/orders/{id}");
        assert_eq!(value["path_raw"], "/orders/7");
        assert_eq!(value["status_code"], 201);
        assert_eq!(value["environment"], "production");
        assert_eq!(value["spans"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn job_transaction_payload() {
        let txn = Transaction::job("ReceiptJob", "mailers").duration_ms(40.0);
        let value = txn.payload("production", || "t".to_string());
        assert_eq!(value["kind"], "job");
        assert_eq!(value["job_class"], "ReceiptJob");
        assert_eq!(value["queue"], "mailers");
    }
}
