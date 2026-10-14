//! Hooks, ignored environments, rate-limit pauses and nested filtering.

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::routing::post;
use axum::Router;
use errorgap::{Client, Configuration, NoticeOptions};
use serde_json::{json, Value};

type Bodies = Arc<Mutex<Vec<Value>>>;

/// A server answering every request with `status` (and `Retry-After: 120`
/// for a 429), recording each body.
async fn stub(status: u16) -> (String, Bodies) {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = bodies.clone();
    let app = Router::new().route(
        "/api/projects/demo/notices",
        post(move |body: String| {
            let recorded = recorded.clone();
            async move {
                recorded
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                let mut headers = HeaderMap::new();
                if status == 429 {
                    headers.insert("retry-after", HeaderValue::from_static("120"));
                }
                (StatusCode::from_u16(status).unwrap(), headers, "{}")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, bodies)
}

fn config(endpoint: &str) -> errorgap::ConfigurationBuilder {
    Configuration::builder()
        .endpoint(endpoint)
        .project_slug("demo")
        .is_async(false)
}

#[tokio::test]
async fn a_429_pauses_sending_for_retry_after() {
    let (endpoint, bodies) = stub(429).await;
    let client = Client::new(config(&endpoint).build().unwrap()).unwrap();

    let first = client.notify_sync("one", NoticeOptions::default()).await;
    assert_eq!(first.status, Some(429));
    assert!(client.paused());

    let second = client.notify_sync("two", NoticeOptions::default()).await;
    assert!(matches!(
        second.error,
        Some(errorgap::ErrorgapError::RateLimited)
    ));
    assert_eq!(bodies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn before_notify_can_drop_or_change_and_a_panicking_hook_is_skipped() {
    let (endpoint, bodies) = stub(201).await;

    let drop = Client::new(config(&endpoint).before_notify(|_| false).build().unwrap()).unwrap();
    let dropped = drop.notify_sync("no", NoticeOptions::default()).await;
    assert_eq!(dropped.status, Some(204));

    let change = Client::new(
        config(&endpoint)
            .before_notify(|notice| {
                notice["context"]["team"] = json!("billing");
                true
            })
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(change
        .notify_sync("yes", NoticeOptions::default())
        .await
        .success());

    let broken = Client::new(
        config(&endpoint)
            .before_notify(|_| panic!("bug in the hook"))
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(broken
        .notify_sync("still sent", NoticeOptions::default())
        .await
        .success());

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["context"]["team"], "billing");
    assert_eq!(bodies[1]["errors"][0]["message"], "still sent");
}

#[tokio::test]
async fn nothing_is_sent_in_an_ignored_environment() {
    let (endpoint, bodies) = stub(201).await;
    let client = Client::new(
        config(&endpoint)
            .environment("test")
            .ignore_environments(["test"])
            .build()
            .unwrap(),
    )
    .unwrap();

    let result = client.notify_sync("x", NoticeOptions::default()).await;
    assert_eq!(result.status, Some(204));
    assert!(bodies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sensitive_keys_are_masked_inside_arrays_and_in_the_session() {
    let (endpoint, bodies) = stub(201).await;
    let client = Client::new(config(&endpoint).build().unwrap()).unwrap();

    let mut options = NoticeOptions::default();
    options
        .params
        .insert("users".into(), json!([{ "password": "p", "name": "Ada" }]));
    options.session.insert("auth_token".into(), json!("t"));
    client.notify_sync("x", options).await;

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies[0]["params"]["users"][0]["password"], "[FILTERED]");
    assert_eq!(bodies[0]["params"]["users"][0]["name"], "Ada");
    assert_eq!(bodies[0]["session"]["auth_token"], "[FILTERED]");
}

#[tokio::test]
async fn remote_settings_switch_a_kind_off_and_fail_open() {
    let app = Router::new()
        .route(
            "/api/projects/demo/config",
            axum::routing::get(|| async {
                r#"{"errors_enabled":false,"apm_enabled":true,"poll_interval_seconds":600}"#
            }),
        )
        .route(
            "/api/projects/demo/notices",
            post(|| async { (StatusCode::CREATED, "{}") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = Client::new(config(&endpoint).remote_settings(true).build().unwrap()).unwrap();
    assert!(client.refresh_remote_settings().await);
    let dropped = client.notify_sync("x", NoticeOptions::default()).await;
    assert_eq!(dropped.status, Some(204), "errors are switched off");

    // Unreachable: nothing is read, and everything stays on.
    let down = Client::new(
        config("http://127.0.0.1:1")
            .remote_settings(true)
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(!down.refresh_remote_settings().await);
    let attempted = down.notify_sync("x", NoticeOptions::default()).await;
    assert!(attempted.error.is_some(), "sent (and refused), not skipped");
}
