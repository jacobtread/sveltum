use std::{
    convert::Infallible,
    path::PathBuf,
    str::FromStr,
    task::{Context, Poll},
};

use axum_core::body::Body;
use deno_runtime::deno_core::futures::future::BoxFuture;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::BodyExt;
use tower_service::Service;

use crate::runtime::{HttpRequest, SvelteServerHandle, SvelteServerRuntime};

#[derive(Clone)]
pub struct ServeSvelte {
    handle: SvelteServerHandle,
    server_path: PathBuf,
}

impl ServeSvelte {
    pub fn create(server_path: PathBuf) -> anyhow::Result<ServeSvelte> {
        let handle = SvelteServerRuntime::create(server_path.clone())?;
        Ok(Self {
            handle,
            server_path,
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

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let handle = self.handle.clone();
        let server_path = self.server_path.clone();
        // TODO: Handle pre-render

        Box::pin(async move {
            let origin = "http://localhost:3000";
            let uri = req.uri().to_owned();
            let url = format!("{}{}", origin, uri);
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
            dbg!(&request);

            let path = uri.path();
            let client_path = server_path
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

            let static_path = server_path
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
