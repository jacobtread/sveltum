use bytes::Bytes;
use fs::{
    async_read_body::AsyncReadBody,
    content_encoding::{Encoding, encodings},
    open_file::{FileOpened, FileRequestExtent, OpenFileOutput, OpenFileSettings, open_file},
};
use headers::{IfModifiedSince, IfUnmodifiedSince};
use percent_encoding::percent_decode;
use std::{
    collections::HashSet,
    convert::Infallible,
    io,
    path::{Component, Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use axum_core::body::Body;
use http::{
    HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header, request::Parts,
};
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use sveltum::{
    core::HttpRequest,
    runtime::{SvelteServerHandle, SvelteServerRuntime},
};
use tower_service::Service;

mod fs;
mod headers;

// default capacity 64KiB
const DEFAULT_CAPACITY: usize = 65536;

#[derive(Clone)]
pub struct ServeSvelte {
    inner: Arc<ServeSvelteInner>,
}

struct ServeSvelteInner {
    handle: SvelteServerHandle,

    client_path: PathBuf,
    static_path: PathBuf,
    prerendered_path: PathBuf,

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

                client_path,
                static_path,
                prerendered_path,

                state,
                config,
            }),
        })
    }
}

impl ServeSvelte {
    fn build_and_validate_path(base_path: &Path, requested_path: &str) -> Option<PathBuf> {
        let path = requested_path.trim_start_matches('/');

        let path_decoded = percent_decode(path.as_ref()).decode_utf8().ok()?;
        let path_decoded = Path::new(&*path_decoded);

        let mut path_to_file = base_path.to_path_buf();
        for component in path_decoded.components() {
            match component {
                Component::Normal(comp) => {
                    // protect against paths like `/foo/c:/bar/baz` (#204)
                    if Path::new(&comp)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_)))
                    {
                        path_to_file.push(comp)
                    } else {
                        return None;
                    }
                }
                Component::CurDir => {}
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return None;
                }
            }
        }
        Some(path_to_file)
    }

    async fn try_serve_from_path(
        parts: &Parts,
        settings: &OpenFileSettings,
        base: &Path,
    ) -> Option<Response<Body>> {
        let path_to_file = Self::build_and_validate_path(base, parts.uri.path())?;

        let outcome = match open_file(settings, path_to_file, parts).await {
            Ok(value) => value,
            Err(err) => {
                #[cfg(unix)]
                // 20 = libc::ENOTDIR => "not a directory
                // when `io_error_more` landed, this can be changed
                // to checking for `io::ErrorKind::NotADirectory`.
                // https://github.com/rust-lang/rust/issues/86442
                let error_is_not_a_directory = err.raw_os_error() == Some(20);
                #[cfg(not(unix))]
                let error_is_not_a_directory = false;

                return if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) || error_is_not_a_directory
                {
                    None
                } else {
                    Some(response_with_status(StatusCode::INTERNAL_SERVER_ERROR))
                };
            }
        };

        match outcome {
            OpenFileOutput::FileOpened(file_output) => Some(build_response(*file_output)),

            OpenFileOutput::Redirect { location } => {
                let mut res = response_with_status(StatusCode::TEMPORARY_REDIRECT);
                res.headers_mut().insert(http::header::LOCATION, location);
                Some(res)
            }

            OpenFileOutput::FileNotFound => None,
            OpenFileOutput::PreconditionFailed => {
                Some(response_with_status(StatusCode::PRECONDITION_FAILED))
            }

            OpenFileOutput::NotModified => Some(response_with_status(StatusCode::NOT_MODIFIED)),
            OpenFileOutput::InvalidRedirectUri => {
                Some(response_with_status(StatusCode::INTERNAL_SERVER_ERROR))
            }
        }
    }

    async fn try_serve_static(parts: &Parts, this: &ServeSvelteInner) -> Option<Response<Body>> {
        let request_path = parts.uri.path();
        let if_unmodified_since = parts
            .headers
            .get(header::IF_UNMODIFIED_SINCE)
            .and_then(IfUnmodifiedSince::from_header_value);

        let if_modified_since = parts
            .headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(IfModifiedSince::from_header_value);

        let buf_chunk_size = DEFAULT_CAPACITY;
        // let buf_chunk_size = self.buf_chunk_size;
        let range_header = parts
            .headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .map(|s| s.to_owned());

        let negotiated_encodings: Vec<_> = encodings(&parts.headers).collect();

        let settings = OpenFileSettings {
            if_unmodified_since,
            if_modified_since,
            negotiated_encodings,
            range_header,
            buf_chunk_size,
        };

        // Try serve client
        if let Some(mut cr) = Self::try_serve_from_path(parts, &settings, &this.client_path).await {
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
        if let Some(cr) = Self::try_serve_from_path(parts, &settings, &this.static_path).await {
            println!("served from static");
            return Some(cr);
        }

        // Serve pre-rendered content
        if this.state.prerendered.contains(request_path) {
            // Try serve static
            if let Some(cr) =
                Self::try_serve_from_path(parts, &settings, &this.prerendered_path).await
            {
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
            if let Some(cr) =
                Self::try_serve_from_path(&parts, &settings, &this.prerendered_path).await
            {
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
                    println!("{error:?}");
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
    println!("{parts:?}");

    let method = parts.method;

    // Collect headers from incoming request
    let request_headers = parts.headers;
    let mut headers = Vec::with_capacity(request_headers.len());

    for (key, value) in request_headers.iter() {
        headers.push((key.to_string(), value.to_str()?.to_string()));
    }

    let method_string = method.to_string();
    let body = match method {
        Method::GET | Method::HEAD => None,
        _ => {
            // Read the request body
            let body = body.collect().await?;
            let body = body.to_bytes();

            Some(body)
        }
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

fn response_with_status(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(empty_body())
        .unwrap()
}

fn body_from_bytes(bytes: Bytes) -> Body {
    let body = Full::from(bytes).map_err(|err| match err {}).boxed_unsync();
    Body::new(UnsyncBoxBody::new(body))
}

fn empty_body() -> Body {
    let body = Empty::new().map_err(|err| match err {}).boxed_unsync();
    Body::new(UnsyncBoxBody::new(body))
}

fn build_response(output: FileOpened) -> Response<Body> {
    let (maybe_file, size) = match output.extent {
        FileRequestExtent::Full(file, meta) => (Some(file), meta.len()),
        FileRequestExtent::Head(meta) => (None, meta.len()),
    };

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, output.mime_header_value)
        .header(header::ACCEPT_RANGES, "bytes");

    if let Some(encoding) = output
        .maybe_encoding
        .filter(|encoding| *encoding != Encoding::Identity)
    {
        builder = builder.header(header::CONTENT_ENCODING, encoding.into_header_value());
    }

    if let Some(last_modified) = output.last_modified {
        builder = builder.header(header::LAST_MODIFIED, last_modified.0.to_string());
    }

    match output.maybe_range {
        Some(Ok(ranges)) => {
            if let Some(range) = ranges.first() {
                if ranges.len() > 1 {
                    builder
                        .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .body(body_from_bytes(Bytes::from(
                            "Cannot serve multipart range requests",
                        )))
                        .unwrap()
                } else {
                    let body = if let Some(file) = maybe_file {
                        let range_size = range.end() - range.start() + 1;
                        Body::new(UnsyncBoxBody::new(
                            AsyncReadBody::with_capacity_limited(
                                file,
                                output.chunk_size,
                                range_size,
                            )
                            .boxed_unsync(),
                        ))
                    } else {
                        empty_body()
                    };

                    let content_length = if size == 0 {
                        0
                    } else {
                        range.end() - range.start() + 1
                    };

                    builder
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", range.start(), range.end(), size),
                        )
                        .header(header::CONTENT_LENGTH, content_length)
                        .status(StatusCode::PARTIAL_CONTENT)
                        .body(body)
                        .unwrap()
                }
            } else {
                builder
                    .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .body(body_from_bytes(Bytes::from(
                        "No range found after parsing range header, please file an issue",
                    )))
                    .unwrap()
            }
        }

        Some(Err(_)) => builder
            .header(header::CONTENT_RANGE, format!("bytes */{}", size))
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .body(empty_body())
            .unwrap(),

        // Not a range request
        None => {
            let body = if let Some(file) = maybe_file {
                Body::new(UnsyncBoxBody::new(
                    AsyncReadBody::with_capacity(file, output.chunk_size).boxed_unsync(),
                ))
            } else {
                empty_body()
            };

            builder
                .header(header::CONTENT_LENGTH, size.to_string())
                .body(body)
                .unwrap()
        }
    }
}

/*
       let future = self
            .try_call(req)
            .map(|result: Result<_, _>| -> Result<_, Infallible> {
                let response = result.unwrap_or_else(|err| {
                    tracing::error!(error = %err, "Failed to read file");

                    let body = ResponseBody::new(UnsyncBoxBody::new(
                        Empty::new().map_err(|err| match err {}).boxed_unsync(),
                    ));
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(body)
                        .unwrap()
                });
                Ok(response)
            } as _);

        InfallibleResponseFuture::new(future)
*/
