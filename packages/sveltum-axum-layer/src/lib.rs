use self::{
    async_read_body::AsyncReadBody,
    content_encoding::{Encoding, encodings},
};
use bytes::Bytes;
use headers::{IfModifiedSince, IfUnmodifiedSince, LastModified};
use percent_encoding::percent_decode;
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
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use http_range_header::RangeUnsatisfiableError;
use std::{ffi::OsStr, fs::Metadata, ops::RangeInclusive};
use sveltum::{
    core::HttpRequest,
    runtime::{SvelteServerHandle, SvelteServerRuntime},
};
use tokio::fs::File;
use tower_service::Service;

mod async_read_body;
mod content_encoding;
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

async fn is_dir(path_to_file: &Path) -> bool {
    tokio::fs::metadata(path_to_file)
        .await
        .is_ok_and(|meta_data| meta_data.is_dir())
}

fn append_slash_on_path(uri: Uri) -> Result<Uri, http::Error> {
    let http::uri::Parts {
        scheme,
        authority,
        path_and_query,
        ..
    } = uri.into_parts();

    let mut uri_builder = Uri::builder();

    if let Some(scheme) = scheme {
        uri_builder = uri_builder.scheme(scheme);
    }

    if let Some(authority) = authority {
        uri_builder = uri_builder.authority(authority);
    }

    let uri_builder = if let Some(path_and_query) = path_and_query {
        if let Some(query) = path_and_query.query() {
            uri_builder.path_and_query(format!("{}/?{}", path_and_query.path(), query))
        } else {
            uri_builder.path_and_query(format!("{}/", path_and_query.path()))
        }
    } else {
        uri_builder.path_and_query("/")
    };

    uri_builder.build().map_err(|err| {
        tracing::error!(?err, "redirect uri failed to build");
        err
    })
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

    let settings = ServeFileOptions {
        if_unmodified_since,
        if_modified_since,
        negotiated_encodings,
        range_header,
        buf_chunk_size,
    };

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

        println!("served from client");
        return Some(cr);
    }

    // Try serve static
    if let Some(cr) = try_serve_from_path(parts, &settings, &this.static_path).await {
        println!("served from static");
        return Some(cr);
    }

    // Serve pre-rendered content
    if this.state.prerendered.contains(request_path) {
        // Try serve static
        if let Some(cr) = try_serve_from_path(parts, &settings, &this.prerendered_path).await {
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
        if let Some(cr) = try_serve_from_path(&parts, &settings, &this.prerendered_path).await {
            println!("served from prerendered (.html)");
            return Some(cr);
        }
    }

    None
}

async fn try_serve_from_path(
    parts: &Parts,
    settings: &ServeFileOptions,
    base: &Path,
) -> Option<Response<Body>> {
    let mut path_to_file = build_and_validate_path(base, parts.uri.path())?;

    // Might already at this point know a redirect or not found result should be
    // returned which corresponds to a Some(output). Otherwise the path might be
    // modified and proceed to the open file/metadata future.
    if is_dir(&path_to_file).await {
        if !parts.uri.path().ends_with('/') {
            let uri = match append_slash_on_path(parts.uri.clone()) {
                Ok(uri) => uri,
                Err(err) => return Some(response_with_status(StatusCode::INTERNAL_SERVER_ERROR)),
            };
            let location = HeaderValue::from_str(&uri.to_string()).unwrap();
            let mut res = response_with_status(StatusCode::TEMPORARY_REDIRECT);
            res.headers_mut().insert(http::header::LOCATION, location);
            return Some(res);
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

struct ServeFileOptions {
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_modified_since: Option<IfModifiedSince>,
    negotiated_encodings: Vec<(Encoding, QValue)>,
    range_header: Option<String>,
    buf_chunk_size: usize,
}

async fn try_open_file(
    path_to_file: PathBuf,
    negotiated_encodings: &[(Encoding, QValue)],
    head: bool,
) -> std::io::Result<(Option<File>, Metadata, Option<Encoding>)> {
    if head {
        let (meta, maybe_encoding) =
            file_metadata_with_fallback(path_to_file, negotiated_encodings.to_vec()).await?;
        return Ok((None, meta, maybe_encoding));
    }

    let (file, maybe_encoding) =
        open_file_with_fallback(path_to_file, negotiated_encodings.to_vec()).await?;
    let meta = file.metadata().await?;
    Ok((Some(file), meta, maybe_encoding))
}

// Returns the preferred_encoding encoding and modifies the path extension
// to the corresponding file extension for the encoding.
fn preferred_encoding(
    path: &mut PathBuf,
    negotiated_encoding: &[(Encoding, QValue)],
) -> Option<Encoding> {
    let preferred_encoding = Encoding::preferred_encoding(negotiated_encoding.iter().copied());

    if let Some(file_extension) =
        preferred_encoding.and_then(|encoding| encoding.to_file_extension())
    {
        let new_file_name = path
            .file_name()
            .map(|file_name| {
                let mut os_string = file_name.to_os_string();
                os_string.push(file_extension);
                os_string
            })
            .unwrap_or_else(|| file_extension.to_os_string());

        path.set_file_name(new_file_name);
    }

    preferred_encoding
}

// Attempts to open the file with any of the possible negotiated_encodings in the
// preferred order. If none of the negotiated_encodings have a corresponding precompressed
// file the uncompressed file is used as a fallback.
async fn open_file_with_fallback(
    mut path: PathBuf,
    mut negotiated_encoding: Vec<(Encoding, QValue)>,
) -> io::Result<(File, Option<Encoding>)> {
    let (file, encoding) = loop {
        // Get the preferred encoding among the negotiated ones.
        let encoding = preferred_encoding(&mut path, &negotiated_encoding);
        match (File::open(&path).await, encoding) {
            (Ok(file), maybe_encoding) => break (file, maybe_encoding),
            (Err(err), Some(encoding)) if err.kind() == io::ErrorKind::NotFound => {
                // Remove the extension corresponding to a precompressed file (.gz, .br, .zz)
                // to reset the path before the next iteration.
                path.set_extension(OsStr::new(""));
                // Remove the encoding from the negotiated_encodings since the file doesn't exist
                negotiated_encoding
                    .retain(|(negotiated_encoding, _)| *negotiated_encoding != encoding);
            }
            (Err(err), _) => return Err(err),
        }
    };
    Ok((file, encoding))
}

// Attempts to get the file metadata with any of the possible negotiated_encodings in the
// preferred order. If none of the negotiated_encodings have a corresponding precompressed
// file the uncompressed file is used as a fallback.
async fn file_metadata_with_fallback(
    mut path: PathBuf,
    mut negotiated_encoding: Vec<(Encoding, QValue)>,
) -> io::Result<(Metadata, Option<Encoding>)> {
    let (file, encoding) = loop {
        // Get the preferred encoding among the negotiated ones.
        let encoding = preferred_encoding(&mut path, &negotiated_encoding);
        match (tokio::fs::metadata(&path).await, encoding) {
            (Ok(file), maybe_encoding) => break (file, maybe_encoding),
            (Err(err), Some(encoding)) if err.kind() == io::ErrorKind::NotFound => {
                // Remove the extension corresponding to a precompressed file (.gz, .br, .zz)
                // to reset the path before the next iteration.
                path.set_extension(OsStr::new(""));
                // Remove the encoding from the negotiated_encodings since the file doesn't exist
                negotiated_encoding
                    .retain(|(negotiated_encoding, _)| *negotiated_encoding != encoding);
            }
            (Err(err), _) => return Err(err),
        }
    };
    Ok((file, encoding))
}

fn try_parse_range(
    maybe_range_ref: Option<&str>,
    file_size: u64,
) -> Option<Result<Vec<RangeInclusive<u64>>, RangeUnsatisfiableError>> {
    maybe_range_ref.map(|header_value| {
        http_range_header::parse_range_header(header_value)
            .and_then(|first_pass| first_pass.validate(file_size))
    })
}
