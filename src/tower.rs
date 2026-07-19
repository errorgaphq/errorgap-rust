//! Tower middleware that reports service errors to Errorgap.
//!
//! Enabled by the `tower` feature (default). Wrap your service with
//! [`ErrorgapLayer`] and any error returned by the inner service — typically
//! from `tonic` or a custom tower stack — is reported.
//!
//! For `axum` and `hyper` the service's error type is usually `Infallible`,
//! so this layer is a no-op there; use the [`NoticeOptions`](crate::NoticeOptions)
//! `notify` API from your fallback handler instead.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use crate::{notify, NoticeOptions};

/// Tower layer that reports inner-service errors via the package-level client.
#[derive(Debug, Clone, Default)]
pub struct ErrorgapLayer;

impl ErrorgapLayer {
    /// Construct a new layer.
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for ErrorgapLayer {
    type Service = ErrorgapService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ErrorgapService { inner }
    }
}

/// Service wrapped by [`ErrorgapLayer`].
#[derive(Debug, Clone)]
pub struct ErrorgapService<S> {
    inner: S,
}

impl<S, Request> Service<Request> for ErrorgapService<S>
where
    S: Service<Request>,
    S::Error: std::fmt::Display,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ErrorgapFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        ErrorgapFuture {
            inner: self.inner.call(request),
        }
    }
}

pin_project! {
    /// Future returned by [`ErrorgapService`].
    pub struct ErrorgapFuture<F> {
        #[pin]
        inner: F,
    }
}

impl<F, T, E> Future for ErrorgapFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    type Output = Result<T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(err)) => {
                let opts = NoticeOptions::default().with_context("source", "tower::ErrorgapLayer");
                let _ = notify(&err);
                drop(opts);
                Poll::Ready(Err(err))
            }
        }
    }
}
