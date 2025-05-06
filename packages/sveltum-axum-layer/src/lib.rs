use self::content_encoding::{Encoding, encodings};
use axum_core::body::Body;
use core::{
    ServeSvelteState, create_dynamic_request, create_dynamic_response, get_request_url,
    try_serve_static,
};
use http::{Method, Request, Response, StatusCode};
use std::{
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use sveltum::runtime::{SvelteServerHandle, SvelteServerRuntime};
use tower_service::Service;

mod async_read_body;
mod content_encoding;
mod core;
mod file;
mod headers;
mod response;

pub use core::ServeSvelteConfig;

#[derive(Clone)]
pub struct ServeSvelte {
    inner: Arc<ServeSvelteInner>,
}

struct ServeSvelteInner {
    handle: SvelteServerHandle,

    config: ServeSvelteConfig,
    state: ServeSvelteState,
}

impl ServeSvelte {
    pub async fn create(server_path: PathBuf, config: ServeSvelteConfig) -> anyhow::Result<Self> {
        // Build the paths that we serve from
        let client_path = server_path.join("client");
        let static_path = server_path.join("static");
        let prerendered_path = server_path.join("prerendered");

        // Start the runtime that will handle dynamic requests
        let (response, handle) = SvelteServerRuntime::create(server_path).await?;

        // Determine the immutable assets path
        let immutable_path = format!("{}/immutable", response.app_path);

        let state = ServeSvelteState {
            prerendered: response.prerendered,
            immutable_path: immutable_path.clone(),
            client_path,
            static_path,
            prerendered_path,
        };

        Ok(Self {
            inner: Arc::new(ServeSvelteInner {
                handle,
                state,
                config,
            }),
        })
    }
}

impl Service<Request<Body>> for ServeSvelte {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let this = self.inner.clone();

        Box::pin(async move {
            let (parts, body) = req.into_parts();

            // Handle requests that can possibly be static content
            if parts.method == Method::GET || parts.method == Method::HEAD {
                if let Some(response) = try_serve_static(&parts, &this.config, &this.state).await {
                    return Ok(response);
                }
            }

            // Derive origin early to save on needing to clone the configuration
            let url = match get_request_url(&parts, &this.config) {
                Some(url) => url,
                None => {
                    // Unable to determine origin
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("creating a response with an empty body will never fail"));
                }
            };

            tracing::debug!("serving from dynamic route");

            // Create the dynamic request
            let request = match create_dynamic_request(parts, url, body).await {
                Ok(value) => value,
                Err(cause) => {
                    tracing::error!(?cause, "failed to create dynamic request");

                    // Unable to handle request
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("creating a response with an empty body will never fail"));
                }
            };

            // Handle the request with a dynamic handler
            let response = match this.handle.request(request).await {
                Ok(value) => value,
                Err(cause) => {
                    tracing::error!(?cause, "failed to handle dynamic request");

                    // Unable to handle request
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("creating a response with an empty body will never fail"));
                }
            };

            // Convert the response into an http compatible one
            let response = match create_dynamic_response(response) {
                Ok(value) => value,
                Err(cause) => {
                    tracing::error!(?cause, "failed to create dynamic route response");

                    // Unable to handle request
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("creating a response with an empty body will never fail"));
                }
            };

            Ok(response)
        })
    }
}
