use bytes::Bytes;
use std::{
    collections::HashSet,
    convert::Infallible,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tower_http::services::ServeDir;

use axum_core::body::Body;
use deno_runtime::deno_core::futures::future::BoxFuture;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header};
use http_body_util::{BodyExt, Empty};
use tower_service::Service;

use crate::{
    core::HttpRequest,
    runtime::{SvelteServerHandle, SvelteServerRuntime},
};

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

        // First check the client path

        Ok(Self {
            inner: Arc::new(ServeSvelteInner {
                handle,
                client_layer: ServeDir::new(client_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                static_layer: ServeDir::new(static_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                prerendered_layer: ServeDir::new(prerendered_path)
                    .precompressed_gzip()
                    .precompressed_br(),
                state: ServeSvelteState {
                    prerendered: response.prerendered,
                    immutable_path: immutable_path.clone(),
                },
                config,
            }),
        })
    }
}

impl Service<Request<Body>> for ServeSvelte {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // TODO: Handle pre-render

        let this = self.inner.clone();

        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let request_path = parts.uri.path();

            if parts.method == Method::GET || parts.method == Method::HEAD {
                // Try serve client
                let mut cr = this
                    .client_layer
                    .clone()
                    .call(Request::from_parts(parts.clone(), Empty::<Bytes>::new()))
                    .await
                    .unwrap();

                if cr.status() != StatusCode::NOT_FOUND {
                    // For successful requests within the immutable path include the immutable cache control header
                    if cr.status().is_success()
                        && request_path.starts_with(&this.state.immutable_path)
                    {
                        let headers = cr.headers_mut();
                        headers.append(
                            header::CACHE_CONTROL,
                            HeaderValue::from_static("public,max-age=31536000,immutable"),
                        );
                    }

                    println!("served from client");
                    return Ok(cr.map(Body::new));
                }

                // Try serve static
                let cr = this
                    .static_layer
                    .clone()
                    .call(Request::from_parts(parts.clone(), Empty::<Bytes>::new()))
                    .await
                    .unwrap();
                if cr.status() != StatusCode::NOT_FOUND {
                    println!("served from static");
                    return Ok(cr.map(Body::new));
                }

                // Serve pre-rendered content
                if this.state.prerendered.contains(request_path) {
                    let cr = this
                        .prerendered_layer
                        .clone()
                        .call(Request::from_parts(parts.clone(), Empty::<Bytes>::new()))
                        .await
                        .unwrap();
                    if cr.status() != StatusCode::NOT_FOUND {
                        println!("served from prerendered");
                        return Ok(cr.map(Body::new));
                    }

                    let mut parts = parts.clone();

                    // Append a path segment
                    let new_path = format!(
                        "{}.html",
                        request_path.strip_suffix("/").unwrap_or(request_path)
                    );
                    let new_uri = Uri::builder().path_and_query(new_path).build().unwrap();
                    parts.uri = new_uri;

                    let cr = this
                        .prerendered_layer
                        .clone()
                        .call(Request::from_parts(parts, Empty::<Bytes>::new()))
                        .await
                        .unwrap();
                    if cr.status() != StatusCode::NOT_FOUND {
                        println!("served from prerendered");
                        return Ok(cr.map(Body::new));
                    }
                }
            }

            // Rebuild original request
            let req = Request::from_parts(parts, body);

            // Derive origin early to save on needing to clone the configuration
            let uri = req.uri();
            let url = match this.config.origin.as_ref() {
                Some(origin) => format!("{origin}{uri}"),
                None => {
                    let derived_origin = get_request_origin(&req);
                    match derived_origin {
                        Some(origin) => format!("{origin}{uri}"),
                        None => {
                            panic!("error, unable to derive request origin")
                        }
                    }
                }
            };

            println!("served from dynamic");

            // Perform a dynamic request
            Ok(dynamic_serve(&this.handle, url, req).await.unwrap())
        })
    }
}

pub fn get_request_origin(req: &Request<Body>) -> Option<&str> {
    let origin = req.headers().get(header::ORIGIN)?;
    origin.to_str().ok()
}

pub fn try_serve_from(
    base_path: &Path,
    immutable_path: &str,
    request_path: &str,
    client: bool,
) -> Option<BoxFuture<'static, Result<Response<Body>, Infallible>>> {
    let client_path = base_path.join(request_path);
    if !client_path.is_file() {
        return None;
    }

    let immutable = client && request_path.starts_with(immutable_path);

    Some(Box::pin(async move {
        return Ok(static_serve(&client_path, immutable).await.unwrap());
    }))
}

/// Serve something statically
pub async fn static_serve(path: &Path, immutable: bool) -> anyhow::Result<Response<Body>> {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let file = tokio::fs::read(path).await?;
    let mut response = Response::builder().header(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.essence_str())?,
    );

    // Append immutable cache header for immutable resources
    if immutable {
        response = response.header(header::CACHE_CONTROL, "public,max-age=31536000,immutable");
    }

    Ok(response.body(file.into())?)
}

pub async fn dynamic_serve(
    handle: &SvelteServerHandle,
    url: String,
    req: Request<Body>,
) -> anyhow::Result<Response<Body>> {
    let mut request = HttpRequest {
        url,
        method: req.method().to_string(),
        headers: req
            .headers()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_str().unwrap().to_string()))
            .collect(),
        body: None,
    };

    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let body = req.into_body();
        let buffer = body.collect().await.unwrap().to_bytes();
        println!("{:?}", buffer);
        request.body = Some(buffer.to_vec().into_boxed_slice())
    }

    let response = handle.request(request).await.unwrap();
    let mut http_response =
        Response::builder().status(StatusCode::from_u16(response.status).unwrap());

    for (name, value) in response.headers {
        http_response = http_response.header(
            HeaderName::from_str(&name).unwrap(),
            HeaderValue::from_str(&value).unwrap(),
        );
    }

    let http_response = http_response.body(response.body.into()).unwrap();

    Ok(http_response)
}
