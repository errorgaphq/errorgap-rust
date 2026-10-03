//! Integration tests against an in-process hyper fake ingestor.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use errorgap::{Client, Configuration, NoticeOptions};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

#[derive(Default)]
struct CaptureState {
    requests: Mutex<Vec<CapturedRequest>>,
}

impl CaptureState {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
    fn record(&self, req: CapturedRequest) {
        self.requests.lock().unwrap().push(req);
    }
}

async fn start_ingestor() -> (SocketAddr, Arc<CaptureState>, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = CaptureState::new();
    let state_clone = state.clone();
    let (tx, mut rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let state_for_conn = state_clone.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: Request<Incoming>| {
                            let state = state_for_conn.clone();
                            async move {
                                let method = req.method().to_string();
                                let path = req.uri().path().to_string();
                                let mut headers: Vec<(String, String)> = req
                                    .headers()
                                    .iter()
                                    .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or("").to_string()))
                                    .collect();
                                headers.sort();
                                let body_bytes = req.into_body().collect().await.unwrap().to_bytes();
                                let body_json: serde_json::Value =
                                    serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
                                state.record(CapturedRequest {
                                    method,
                                    path,
                                    headers,
                                    body: body_json,
                                });
                                let response = Response::builder()
                                    .status(StatusCode::CREATED)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(b"{\"group_id\":\"g_1\"}")))
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = http1::Builder::new().serve_connection(io, svc).await;
                    });
                }
            }
        }
    });

    (addr, state, tx)
}

#[tokio::test]
async fn posts_to_notices_with_canonical_headers() {
    let (addr, state, stop) = start_ingestor().await;

    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("flk_test")
        .is_async(true)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();

    let result = client.notify("test", NoticeOptions::default());
    assert!(result.queued);
    assert_eq!(result.status, Some(202));

    client.flush().await;

    let reqs = state.requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/api/projects/demo/notices");
    let header_map: std::collections::HashMap<_, _> = req.headers.iter().cloned().collect();
    assert_eq!(
        header_map.get("x-errorgap-project-key").map(|s| s.as_str()),
        Some("flk_test")
    );
    assert!(header_map
        .get("user-agent")
        .map(|s| s.starts_with("errorgap-rust/"))
        .unwrap_or(false));

    let _ = stop.send(());
}

#[tokio::test]
async fn sends_full_notice_envelope() {
    let (addr, state, stop) = start_ingestor().await;

    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("flk_test")
        .is_async(true)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();

    client.notify("kaboom", NoticeOptions::default());
    client.flush().await;

    let req = state.requests().pop().expect("at least one request");
    let body = req.body.as_object().expect("body is object");
    assert!(body.contains_key("errors"));
    assert!(body.contains_key("context"));
    let errors = body.get("errors").unwrap().as_array().unwrap();
    let first = errors[0].as_object().unwrap();
    assert_eq!(first.get("message").unwrap().as_str(), Some("kaboom"));

    let _ = stop.send(());
}

#[tokio::test]
async fn rejects_missing_project_slug() {
    let result = Configuration::builder()
        .endpoint("https://errorgap.example.com")
        .build();
    assert!(matches!(
        result,
        Err(errorgap::ErrorgapError::MissingProjectSlug)
    ));
}

#[derive(Debug)]
struct RootError;

impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("orders database unreachable")
    }
}

impl std::error::Error for RootError {}

#[derive(Debug)]
struct CheckoutError {
    source: RootError,
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("checkout failed")
    }
}

impl std::error::Error for CheckoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[tokio::test]
async fn records_cause_chain_in_context() {
    let (addr, state, stop) = start_ingestor().await;

    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("flk_test")
        .is_async(true)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();

    client.notify_error(
        &CheckoutError { source: RootError },
        NoticeOptions::default(),
    );
    client.flush().await;

    let req = state.requests().pop().expect("a request");
    let body = req.body.as_object().unwrap();
    let errors = body.get("errors").unwrap().as_array().unwrap();
    assert_eq!(errors[0]["type"], "CheckoutError");
    assert_eq!(errors[0]["message"], "checkout failed");
    let causes = body["context"]["causes"].as_array().unwrap();
    assert_eq!(causes.len(), 1);
    assert_eq!(causes[0]["type"], "RootError");
    assert_eq!(causes[0]["message"], "orders database unreachable");

    let _ = stop.send(());
}

#[tokio::test]
async fn posts_apm_transaction() {
    let (addr, state, stop) = start_ingestor().await;

    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("flk_test")
        .is_async(true)
        .apm_enabled(true)
        .apm_sample_rate(1.0)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();

    let mut spans = errorgap::SpanCollector::new();
    spans.database("SELECT * FROM orders WHERE id = 7", 4.0);
    spans.external(30.0);
    let txn = errorgap::Transaction::web("GET", "/orders/{id}", "/orders/7")
        .status_code(200)
        .duration_ms(42.0)
        .spans(spans);

    let result = client.notify_transaction(txn);
    assert!(result.queued);
    client.flush().await;

    let req = state.requests().pop().expect("a request");
    assert_eq!(req.path, "/api/projects/demo/transactions");
    assert_eq!(req.body["kind"], "web");
    assert_eq!(req.body["path"], "/orders/{id}");
    assert_eq!(req.body["path_raw"], "/orders/7");
    assert_eq!(req.body["spans"].as_array().unwrap().len(), 2);

    let _ = stop.send(());
}

#[tokio::test]
async fn skips_apm_when_disabled() {
    let config = Configuration::builder()
        .endpoint("http://127.0.0.1:1")
        .project_slug("demo")
        .apm_enabled(false)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();
    let result = client.notify_transaction(errorgap::Transaction::job("J", "default"));
    assert_eq!(result.status, Some(204));
    assert!(!result.queued);
}

#[tokio::test]
async fn posts_structured_log() {
    let (addr, state, stop) = start_ingestor().await;

    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("flk_test")
        .is_async(true)
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();

    let result = client.notify_log("gateway timeout", "error", Some("payments"));
    assert!(result.queued);
    client.flush().await;

    let req = state.requests().pop().expect("a request");
    assert_eq!(req.path, "/api/projects/demo/logs");
    assert_eq!(req.body["message"], "gateway timeout");
    assert_eq!(req.body["level"], "error");
    assert_eq!(req.body["source"], "payments");

    let _ = stop.send(());
}

#[tokio::test]
async fn drops_log_below_minimum_level() {
    let config = Configuration::builder()
        .endpoint("http://127.0.0.1:1")
        .project_slug("demo")
        .minimum_log_level("warn")
        .build()
        .unwrap();
    let client = Client::new(config).unwrap();
    let result = client.notify_log("chatty", "info", None);
    assert_eq!(result.status, Some(204));
    assert!(!result.queued);
}

fn apm_client(addr: SocketAddr) -> Client {
    let config = Configuration::builder()
        .endpoint(format!("http://{}", addr))
        .project_slug("demo")
        .api_key("egp_test")
        .is_async(true)
        .apm_enabled(true)
        .apm_sample_rate(1.0)
        .build()
        .unwrap();
    Client::new(config).unwrap()
}

/// Errors reported inside a transaction carry its id, so errorgap shows the
/// error the request raised and links the occurrence to its trace.
#[tokio::test]
async fn errors_inside_a_transaction_carry_its_id() {
    let (addr, state, stop) = start_ingestor().await;
    let client = apm_client(addr);

    let txn = errorgap::Transaction::web("GET", "/orders/{id}", "/orders/7");
    let id = txn.id().to_string();
    assert_eq!(id.len(), 36, "a canonical uuid: {id}");

    let inner = client.clone();
    errorgap::in_transaction(id.clone(), async move {
        inner.notify("boom", NoticeOptions::default());
    })
    .await;
    // Outside the scope there is no current transaction.
    client.notify("after", NoticeOptions::default());
    client.notify_transaction(txn.status_code(500).duration_ms(12.0));
    client.flush().await;

    let requests = state.requests();
    let notice = |message: &str| {
        requests
            .iter()
            .find(|r| r.path.ends_with("/notices") && r.body["errors"][0]["message"] == message)
            .expect("notice")
            .body
            .clone()
    };
    assert_eq!(notice("boom")["context"]["transaction_id"], id);
    assert!(notice("after")["context"].get("transaction_id").is_none());
    let transaction = requests
        .iter()
        .find(|r| r.path.ends_with("/transactions"))
        .expect("transaction");
    assert_eq!(transaction.body["id"], id);

    let _ = stop.send(());
}

#[tokio::test]
async fn concurrent_transactions_keep_their_own_ids() {
    let (a, b) = tokio::join!(
        errorgap::in_transaction("a", async {
            tokio::task::yield_now().await;
            errorgap::current_transaction_id()
        }),
        errorgap::in_transaction("b", async {
            tokio::task::yield_now().await;
            errorgap::current_transaction_id()
        }),
    );
    assert_eq!(a.as_deref(), Some("a"));
    assert_eq!(b.as_deref(), Some("b"));
    assert_eq!(errorgap::current_transaction_id(), None);
}

#[test]
fn sync_scopes_nest_and_restore() {
    let seen = errorgap::in_transaction_sync("outer", || {
        let inner = errorgap::in_transaction_sync("inner", errorgap::current_transaction_id);
        (inner, errorgap::current_transaction_id())
    });
    assert_eq!(seen, (Some("inner".into()), Some("outer".into())));
    assert_eq!(errorgap::current_transaction_id(), None);
}

#[tokio::test]
async fn an_explicit_transaction_id_wins() {
    let (addr, state, stop) = start_ingestor().await;
    let client = apm_client(addr);
    let inner = client.clone();
    errorgap::in_transaction("scoped", async move {
        inner.notify(
            "x",
            NoticeOptions::default().with_context("transaction_id", "mine"),
        );
    })
    .await;
    client.flush().await;
    let notice = state.requests().pop().expect("notice");
    assert_eq!(notice.body["context"]["transaction_id"], "mine");
    let _ = stop.send(());
}
