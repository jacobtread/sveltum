use self::{
    async_read_body::AsyncReadBody,
    content_encoding::{Encoding, encodings},
};
use bytes::Bytes;
use file::try_open_file;
use headers::{IfModifiedSince, IfUnmodifiedSince, LastModified, try_parse_range};
use percent_encoding::percent_decode;
use response::{body_from_bytes, empty_body, response_with_status};
use std::{
    collections::HashSet,
    convert::Infallible,
    io::{self, SeekFrom},
    path::{Component, Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::AsyncSeekExt;

use crate::content_encoding::QValue;
use axum_core::body::Body;
use http::HeaderValue;
use http::{HeaderName, Method, Request, Response, StatusCode, Uri, header, request::Parts};
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use sveltum::{
    core::{HttpRequest, HttpResponse},
    runtime::{SvelteServerHandle, SvelteServerRuntime},
};
use tower_service::Service;

mod async_read_body;
mod content_encoding;
mod file;
mod headers;
mod response;

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
    buf_chunk_size: usize,
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
                buf_chunk_size: DEFAULT_CAPACITY,
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
                if let Some(response) = try_serve_static(&parts, &this).await {
                    return Ok(response);
                }
            }

            // Derive origin early to save on needing to clone the configuration
            let url = match this
                .config
                .origin
                .as_deref()
                .or_else(|| {
                    // Attempt to extract the origin from the header
                    parts
                        .headers
                        .get(header::ORIGIN)
                        .and_then(|origin| origin.to_str().ok())
                })
                .map(|origin| format!("{}{}", origin, &parts.uri))
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

/// Translate HTTP request parts and body into a [HttpRequest] that can
/// be processed
async fn create_dynamic_request(
    parts: Parts,
    url: String,
    body: Body,
) -> anyhow::Result<HttpRequest> {
    // Collect headers from incoming request
    let request_headers = parts.headers;

    // Create collection of headers
    let mut headers = Vec::with_capacity(request_headers.len());
    for (key, value) in request_headers.iter() {
        headers.push((key.to_string(), value.to_str()?.to_string()));
    }

    let method_string = parts.method.to_string();

    // Read body for requests that can have
    let body = match parts.method {
        Method::GET | Method::HEAD => None,
        _ => {
            // Read the request body
            let body = body.collect().await?;
            let body = body.to_bytes();

            Some(body)
        }
    };

    Ok(HttpRequest {
        url,
        method: method_string,
        headers,
        body,
    })
}

/// Convert a dynamic response into a http response
fn create_dynamic_response(response: HttpResponse) -> anyhow::Result<Response<Body>> {
    let mut http_response = Response::builder().status(StatusCode::from_u16(response.status)?);

    for (name, value) in response.headers {
        http_response =
            http_response.header(HeaderName::from_str(&name)?, HeaderValue::from_str(&value)?);
    }

    let body = Body::new(Full::new(response.body));
    let http_response = http_response.body(body)?;

    Ok(http_response)
}

struct FileRequestOptions {
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_modified_since: Option<IfModifiedSince>,
    range_header: Option<String>,
    negotiated_encodings: Vec<Encoding>,
    buf_chunk_size: usize,
}

fn get_file_request_options(parts: &Parts, this: &ServeSvelteInner) -> FileRequestOptions {
    let if_unmodified_since = parts
        .headers
        .get(header::IF_UNMODIFIED_SINCE)
        .and_then(IfUnmodifiedSince::from_header_value);

    let if_modified_since = parts
        .headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(IfModifiedSince::from_header_value);

    let range_header = parts
        .headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|s| s.to_owned());

    let buf_chunk_size = this.buf_chunk_size;

    // Collect allowed encodings from the headers
    let mut negotiated_encodings: Vec<(Encoding, QValue)> = encodings(&parts.headers)
        // Only filter to "acceptable" types
        .filter(|(_, value)| value.0 > 0)
        // Collect encodings
        .collect();

    // Sort encodings by most preferred (Descending order, highest Q value first)
    negotiated_encodings.sort_by(|a, b| b.1.cmp(&a.1));

    let negotiated_encodings = negotiated_encodings
        .into_iter()
        .map(|value| value.0)
        .collect();

    FileRequestOptions {
        if_unmodified_since,
        if_modified_since,
        negotiated_encodings,
        range_header,
        buf_chunk_size,
    }
}

async fn try_serve_static(parts: &Parts, this: &ServeSvelteInner) -> Option<Response<Body>> {
    let request_path = parts.uri.path();

    let settings = get_file_request_options(parts, this);

    // Try serve client
    if let Some(mut cr) = try_serve_from_path(parts, &settings, &this.client_path).await {
        // For successful requests within the immutable path include the immutable cache control header
        if cr.status().is_success() && request_path.starts_with(&this.state.immutable_path) {
            let headers = cr.headers_mut();
            headers.append(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public,max-age=31536000,immutable"),
            );
        }

        tracing::debug!("serving from client path");
        return Some(cr);
    }

    // Try serve static
    if let Some(cr) = try_serve_from_path(parts, &settings, &this.static_path).await {
        tracing::debug!("serving from static path");
        return Some(cr);
    }

    // Serve pre-rendered content
    if this.state.prerendered.contains(request_path) {
        // Try serve static
        if let Some(cr) = try_serve_from_path(parts, &settings, &this.prerendered_path).await {
            tracing::debug!("serving from prerendered path");
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
        if let Some(cr) = try_serve_from_path(&parts, &settings, &this.prerendered_path).await {
            tracing::debug!("serving from prerendered path (html extension)");
            return Some(cr);
        }
    }

    None
}

async fn try_serve_from_path(
    parts: &Parts,
    settings: &FileRequestOptions,
    base: &Path,
) -> Option<Response<Body>> {
    let mut path_to_file = build_and_validate_path(base, parts.uri.path())?;

    if tokio::fs::metadata(&path_to_file)
        .await
        .is_ok_and(|meta_data| meta_data.is_dir())
    {
        // Cannot serve a directory when referenced by name
        if !parts.uri.path().ends_with('/') {
            return None;
        }

        // Append index.html searching
        path_to_file.push("index.html");
    }

    let mime = mime_guess::from_path(&path_to_file)
        .first_raw()
        .map(HeaderValue::from_static)
        .unwrap_or_else(|| HeaderValue::from_str(mime::APPLICATION_OCTET_STREAM.as_ref()).unwrap());

    let (maybe_file, meta, maybe_encoding) = match try_open_file(
        path_to_file,
        &settings.negotiated_encodings,
        parts.method == Method::HEAD,
    )
    .await
    {
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

    let size = meta.len();
    let last_modified = meta.modified().ok().map(LastModified::from);

    // Check if the if-unmodified-since header condition is met
    if let Some(since) = settings.if_unmodified_since.as_ref() {
        let precondition = last_modified
            .as_ref()
            .map(|time| since.precondition_passes(time))
            .unwrap_or(false);

        if !precondition {
            return Some(response_with_status(StatusCode::PRECONDITION_FAILED));
        }
    }

    // Check if the if-modified-since header condition is met
    if let Some(since) = settings.if_modified_since.as_ref() {
        let unmodified = last_modified
            .as_ref()
            .map(|time| !since.is_modified(time))
            // no last_modified means its always modified
            .unwrap_or(false);
        if unmodified {
            return Some(response_with_status(StatusCode::NOT_MODIFIED));
        }
    }

    let maybe_range = try_parse_range(settings.range_header.as_deref(), size);

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes");

    if let Some(encoding) = maybe_encoding.filter(|encoding| *encoding != Encoding::Identity) {
        builder = builder.header(header::CONTENT_ENCODING, encoding.into_header_value());
    }

    if let Some(last_modified) = last_modified {
        builder = builder.header(header::LAST_MODIFIED, last_modified.0.to_string());
    }

    match maybe_range {
        // Invalid range
        Some(Err(_)) => Some(
            builder
                .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(empty_body())
                .unwrap(),
        ),

        // Not a range request
        None => {
            let body = if let Some(file) = maybe_file {
                Body::new(UnsyncBoxBody::new(
                    AsyncReadBody::with_capacity(file, settings.buf_chunk_size).boxed_unsync(),
                ))
            } else {
                empty_body()
            };

            Some(
                builder
                    .header(header::CONTENT_LENGTH, size.to_string())
                    .body(body)
                    .unwrap(),
            )
        }

        // Valid range
        Some(Ok(ranges)) => {
            if let Some(range) = ranges.first() {
                // if there is any other amount of ranges than 1 we'll return an
                // unsatisfiable later as there isn't yet support for multipart ranges
                if ranges.len() > 1 {
                    return Some(
                        builder
                            .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                            .status(StatusCode::RANGE_NOT_SATISFIABLE)
                            .body(body_from_bytes(Bytes::from(
                                "Cannot serve multipart range requests",
                            )))
                            .unwrap(),
                    );
                }

                let body = if let Some(mut file) = maybe_file {
                    // Seek to the desired start of the range within the file
                    if let Err(_err) = file.seek(SeekFrom::Start(*ranges[0].start())).await {
                        return Some(response_with_status(StatusCode::INTERNAL_SERVER_ERROR));
                    }

                    let range_size = range.end() - range.start() + 1;
                    Body::new(UnsyncBoxBody::new(
                        AsyncReadBody::with_capacity_limited(
                            file,
                            settings.buf_chunk_size,
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

                return Some(
                    builder
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {}-{}/{}", range.start(), range.end(), size),
                        )
                        .header(header::CONTENT_LENGTH, content_length)
                        .status(StatusCode::PARTIAL_CONTENT)
                        .body(body)
                        .unwrap(),
                );
            }

            Some(
                builder
                    .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .body(body_from_bytes(Bytes::from(
                        "No range found after parsing range header, please file an issue",
                    )))
                    .unwrap(),
            )
        }
    }
}

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
