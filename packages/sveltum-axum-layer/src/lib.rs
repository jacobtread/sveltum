use bytes::Bytes;
use std::{
    collections::HashSet,
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tower_http::services::ServeDir;

use axum_core::body::Body;
use http::{
    HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header, request::Parts,
};
use http_body_util::{BodyExt, Empty, Full};
use sveltum::{
    core::HttpRequest,
    runtime::{SvelteServerHandle, SvelteServerRuntime},
};
use tower_service::Service;

#[derive(Clone)]
pub struct ServeSvelte {
    inner: Arc<ServeSvelteInner>,
}

struct ServeSvelteInner {
    handle: SvelteServerHandle,
    client_layer: ServeDir,
    static_layer: ServeDir,
    prerendered_layer: ServeDir,
    config: ServeSvelteConfig,
    state: ServeSvelteState,
}

struct ServeSvelteState {
    prerendered: HashSet<String>,
    immutable_path: String,
}

pub struct ServeSvelteConfig {
    pub server_path: PathBuf,
    pub origin: Option<String>,
}

impl ServeSvelte {
    pub async fn create(config: ServeSvelteConfig) -> anyhow::Result<Self> {
        let (response, handle) = SvelteServerRuntime::create(config.server_path.clone()).await?;

        let client_path = config.server_path.join("client");
        let static_path = config.server_path.join("static");
        let prerendered_path = config.server_path.join("prerendered");
        let immutable_path = format!("{}/immutable", response.app_path);

        let state = ServeSvelteState {
            prerendered: response.prerendered,
            immutable_path: immutable_path.clone(),
        };

        Ok(Self {
            inner: Arc::new(ServeSvelteInner {
                handle,
                //
                client_layer: ServeDir::new(client_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                static_layer: ServeDir::new(static_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                prerendered_layer: ServeDir::new(prerendered_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                //
                state,
                config,
            }),
        })
    }
}

impl ServeSvelte {
    async fn try_serve_with(parts: Parts, layer: &mut ServeDir) -> Option<Response<Body>> {
        let request = Request::from_parts(parts, Empty::<Bytes>::new());

        // Try and serve with the layer
        let cr = match layer.call(request).await {
            Ok(value) => value,
            Err(err) => match err {},
        };

        // Got a not found response
        if cr.status() == StatusCode::NOT_FOUND {
            return None;
        }

        Some(cr.map(Body::new))
    }

    async fn try_serve_static(parts: &Parts, this: &ServeSvelteInner) -> Option<Response<Body>> {
        let request_path = parts.uri.path();

        // Try serve client
        if let Some(mut cr) =
            Self::try_serve_with(parts.clone(), &mut this.client_layer.clone()).await
        {
            // For successful requests within the immutable path include the immutable cache control header
            if cr.status().is_success() && request_path.starts_with(&this.state.immutable_path) {
                let headers = cr.headers_mut();
                headers.append(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public,max-age=31536000,immutable"),
                );
            }

            println!("served from client");
            return Some(cr);
        }

        // Try serve static
        if let Some(cr) = Self::try_serve_with(parts.clone(), &mut this.static_layer.clone()).await
        {
            println!("served from static");
            return Some(cr);
        }

        // Serve pre-rendered content
        if this.state.prerendered.contains(request_path) {
            let mut prerendered_layer = this.prerendered_layer.clone();

            // Try serve static
            if let Some(cr) = Self::try_serve_with(parts.clone(), &mut prerendered_layer).await {
                println!("served from prerendered");
                return Some(cr);
            }

            let mut parts = parts.clone();

            // Append a path segment
            let path_with_ext = format!(
                "{}.html",
                request_path.strip_suffix("/").unwrap_or(request_path)
            );
            let uri = Uri::builder()
                .path_and_query(path_with_ext)
                .build()
                .expect("changing the path of a url should never fail");
            parts.uri = uri;

            // Try serve static
            if let Some(cr) = Self::try_serve_with(parts, &mut prerendered_layer).await {
                println!("served from prerendered (.html)");
                return Some(cr);
            }
        }

        None
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
                if let Some(response) = Self::try_serve_static(&parts, &this).await {
                    return Ok(response);
                }
            }

            // Rebuild original request
            let req = Request::from_parts(parts, body);

            // Derive origin early to save on needing to clone the configuration
            let url = match this
                .config
                .origin
                .as_deref()
                .or_else(|| get_request_origin(&req))
                .map(|origin| format!("{}{}", origin, req.uri()))
            {
                Some(url) => url,
                None => {
                    // Unable to determine origin
                    return Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .expect("creating a response with an empty body will never fail"));
                }
            };

            // Handle the request with a dynamic handler
            println!("served from dynamic");
            let response = match dynamic_serve(&this.handle, url, req).await {
                Ok(value) => value,
                Err(error) => {
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

fn get_request_origin(req: &Request<Body>) -> Option<&str> {
    let origin = req.headers().get(header::ORIGIN)?;
    origin.to_str().ok()
}

async fn dynamic_serve(
    handle: &SvelteServerHandle,
    url: String,
    req: Request<Body>,
) -> anyhow::Result<Response<Body>> {
    let (parts, body) = req.into_parts();

    let method = parts.method;

    // Collect headers from incoming request
    let request_headers = parts.headers;
    let mut headers = Vec::with_capacity(request_headers.len());

    for (key, value) in request_headers.iter() {
        headers.push((key.to_string(), value.to_str()?.to_string()));
    }

    let method_string = method.to_string();
    let body = match method {
        Method::GET | Method::HEAD => {
            // Read the request body
            let body = body.collect().await?;
            let body = body.to_bytes();

            Some(body)
        }
        _ => None,
    };

    let request = HttpRequest {
        url,
        method: method_string,
        headers,
        body,
    };

    let response = handle.request(request).await?;
    let mut http_response = Response::builder().status(StatusCode::from_u16(response.status)?);

    for (name, value) in response.headers {
        http_response =
            http_response.header(HeaderName::from_str(&name)?, HeaderValue::from_str(&value)?);
    }

    let body = Body::new(Full::new(response.body));
    let http_response = http_response.body(body)?;

    Ok(http_response)
}
