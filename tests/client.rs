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
