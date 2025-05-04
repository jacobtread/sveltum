use std::{
    collections::HashSet,
    convert::Infallible,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use axum_core::body::Body;
use deno_runtime::deno_core::futures::future::BoxFuture;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use tower_service::Service;

use crate::runtime::{HttpRequest, SvelteServerHandle, SvelteServerRuntime};

#[derive(Clone)]
pub struct ServeSvelte {
    handle: SvelteServerHandle,
    config: Arc<ServeSvelteConfig>,
    state: Arc<ServeSvelteState>,
}

struct ServeSvelteState {
    prerendered: HashSet<String>,
    client_path: PathBuf,
    static_path: PathBuf,
    prerendered_path: PathBuf,
    immutable_path: String,
}

pub struct ServeSvelteConfig {
    pub server_path: PathBuf,
    pub origin: Option<String>,
}

impl ServeSvelte {
    pub async fn create(config: ServeSvelteConfig) -> anyhow::Result<ServeSvelte> {
        let (response, handle) = SvelteServerRuntime::create(config.server_path.clone()).await?;

        let client_path = config.server_path.join("client");
        let static_path = config.server_path.join("static");
        let prerendered_path = config.server_path.join("prerendered");
        let immutable_path = format!("{}/immutable", response.app_path);

        Ok(Self {
            handle,
            state: Arc::new(ServeSvelteState {
                prerendered: response.prerendered,
                client_path,
                static_path,
                prerendered_path,
                immutable_path,
            }),
            config: Arc::new(config),
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

        let uri = req.uri();

        let path = uri.path();
        let path_without_prefix = path.strip_prefix("/").unwrap_or(path);

        // Try serving from client directory
        if let Some(future) = try_serve_from(
            &self.state.client_path,
            &self.state.immutable_path,
            path_without_prefix,
            true,
        ) {
            return future;
        }

        // Try serving from static directory
        if let Some(future) = try_serve_from(
            &self.state.static_path,
            &self.state.immutable_path,
            path_without_prefix,
            false,
        ) {
            return future;
        }

        // Serve pre-rendered content
        if self.state.prerendered.contains(path) {
            // Try serving from static directory
            if let Some(future) = try_serve_from(
                &self.state.prerendered_path,
                &self.state.immutable_path,
                path_without_prefix,
                false,
            ) {
                return future;
            }
        }

        // Derive origin early to save on needing to clone the configuration
        let url = match self.config.origin.as_ref() {
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

        let handle = self.handle.clone();

        Box::pin(async move { Ok(dynamic_serve(handle, url, req).await.unwrap()) })
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
    handle: SvelteServerHandle,
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
