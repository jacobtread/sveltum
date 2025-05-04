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
use http::{
    HeaderName, HeaderValue, Method, Request, Response, StatusCode,
    header::{self, CONTENT_TYPE},
};
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
}

pub struct ServeSvelteConfig {
    pub server_path: PathBuf,
    pub origin: Option<String>,
}

impl ServeSvelte {
    pub async fn create(config: ServeSvelteConfig) -> anyhow::Result<ServeSvelte> {
        let (response, handle) = SvelteServerRuntime::create(config.server_path.clone()).await?;

        Ok(Self {
            handle,
            state: Arc::new(ServeSvelteState {
                prerendered: response.prerendered,
            }),
            config: Arc::new(config),
        })
    }
}

pub fn get_request_origin(req: &Request<Body>) -> Option<&str> {
    let origin = req.headers().get(header::ORIGIN)?;
    origin.to_str().ok()
}

/// Serve something statically
pub async fn static_serve(path: &Path) {}

pub async fn static_serve_client(path: &Path) {}

impl Service<Request<Body>> for ServeSvelte {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let handle = self.handle.clone();
        let config = self.config.clone();
        // TODO: Handle pre-render

        Box::pin(async move {
            let uri = req.uri();

            let path = uri.path();
            let client_path = config
                .server_path
                .join("client")
                .join(path.strip_prefix("/").unwrap_or(path));
            println!("CLIENT {}", client_path.display());
            if client_path.is_file() {
                let mime = mime_guess::from_path(&client_path).first_or_octet_stream();
                let file = tokio::fs::read(client_path).await.unwrap();
                return Ok(Response::builder()
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_str(mime.essence_str()).unwrap(),
                    )
                    .body(file.into())
                    .unwrap());
            }

            let static_path = config
                .server_path
                .join("static")
                .join(path.strip_prefix("/").unwrap_or(path));
            println!("STATIC {}", static_path.display());

            if static_path.is_file() {
                let mime = mime_guess::from_path(&static_path).first_or_octet_stream();

                let file = tokio::fs::read(static_path).await.unwrap();
                return Ok(Response::builder()
                    .header(
                        CONTENT_TYPE,
                        HeaderValue::from_str(mime.essence_str()).unwrap(),
                    )
                    .body(file.into())
                    .unwrap());
            }

            let url = match config.origin.as_ref() {
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
        })
    }
}
