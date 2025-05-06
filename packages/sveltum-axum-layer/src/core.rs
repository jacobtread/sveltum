use crate::{
    Encoding,
    async_read_body::AsyncReadBody,
    content_encoding::QValue,
    encodings,
    file::{file_metadata_with_fallback, open_file_with_fallback},
    headers::{IfModifiedSince, IfUnmodifiedSince, LastModified, try_parse_range},
    response::{body_from_bytes, body_from_message, empty_body, response_with_status},
};
use axum_core::body::Body;
use http::{HeaderName, HeaderValue, Method, Response, StatusCode, Uri, header, request::Parts};
use http_body_util::BodyExt;
use http_range_header::RangeUnsatisfiableError;
use percent_encoding::percent_decode_str;
use std::{
    collections::HashSet,
    io::{self, SeekFrom},
    ops::RangeInclusive,
    path::{Component, Path, PathBuf},
    str::FromStr,
};
use sveltum::core::{HttpRequest, HttpResponse};
use tokio::{fs::File, io::AsyncSeekExt};

// default capacity 64KiB
const DEFAULT_CAPACITY: usize = 65536;

pub struct ServeSvelteConfig {
    pub origin: Option<String>,
    pub buf_chunk_size: usize,
}

impl Default for ServeSvelteConfig {
    fn default() -> Self {
        Self {
            origin: None,
            buf_chunk_size: DEFAULT_CAPACITY,
        }
    }
}

pub struct ServeSvelteState {
    // Pre-rendered urls
    pub prerendered: HashSet<String>,

    // Immutable path
    pub immutable_path: String,

    // Paths
    pub client_path: PathBuf,
    pub static_path: PathBuf,
    pub prerendered_path: PathBuf,
}

// Options extracted from a file request
struct FileRequestOptions {
    if_unmodified_since: Option<IfUnmodifiedSince>,
    if_modified_since: Option<IfModifiedSince>,
    range_header: Option<String>,
    negotiated_encodings: Vec<Encoding>,
    buf_chunk_size: usize,
}

impl FileRequestOptions {
    pub fn from_parts(parts: &Parts, config: &ServeSvelteConfig) -> FileRequestOptions {
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

        let buf_chunk_size = config.buf_chunk_size;

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
}

/// Create a URL for a request
pub fn get_request_url(parts: &Parts, config: &ServeSvelteConfig) -> Option<String> {
    config
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
}

/// Translate HTTP request parts and body into a [HttpRequest] that can
/// be processed
pub async fn create_dynamic_request(
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
pub fn create_dynamic_response(response: HttpResponse) -> anyhow::Result<Response<Body>> {
    let mut http_response = Response::builder().status(StatusCode::from_u16(response.status)?);

    for (name, value) in response.headers {
        http_response =
            http_response.header(HeaderName::from_str(&name)?, HeaderValue::from_str(&value)?);
    }

    let body = body_from_bytes(response.body);
    let http_response = http_response.body(body)?;

    Ok(http_response)
}

pub async fn try_serve_static(
    parts: &Parts,
    config: &ServeSvelteConfig,
    state: &ServeSvelteState,
) -> Option<Response<Body>> {
    let request_path = parts.uri.path();

    let settings = FileRequestOptions::from_parts(parts, config);

    // Try serve client
    if let Some(mut cr) = try_serve_from_path(parts, &settings, &state.client_path).await {
        // For successful requests within the immutable path include the immutable cache control header
        if cr.status().is_success() && request_path.starts_with(&state.immutable_path) {
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
    if let Some(cr) = try_serve_from_path(parts, &settings, &state.static_path).await {
        tracing::debug!("serving from static path");
        return Some(cr);
    }

    // Serve pre-rendered content
    if state.prerendered.contains(request_path) {
        // Try serve static
        if let Some(cr) = try_serve_from_path(parts, &settings, &state.prerendered_path).await {
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
        if let Some(cr) = try_serve_from_path(&parts, &settings, &state.prerendered_path).await {
            tracing::debug!("serving from prerendered path (html extension)");
            return Some(cr);
        }
    }

    None
}

fn build_and_validate_path(base_path: &Path, requested_path: &str) -> Option<PathBuf> {
    let path = requested_path.trim_start_matches('/');

    let path_decoded = percent_decode_str(path).decode_utf8().ok()?;
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
        .unwrap_or_else(|| HeaderValue::from_static(mime::APPLICATION_OCTET_STREAM.as_ref()));

    let result = match parts.method {
        Method::HEAD => file_metadata_with_fallback(path_to_file, &settings.negotiated_encodings)
            .await
            .map(|(meta, maybe_encoding)| (None, meta, maybe_encoding)),
        _ => open_file_with_fallback(path_to_file, &settings.negotiated_encodings)
            .await
            .map(|(file, meta, maybe_encoding)| (Some(file), meta, maybe_encoding)),
    };

    let (maybe_file, meta, maybe_encoding) = match result {
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

            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) || error_is_not_a_directory
            {
                return None;
            }

            return Some(response_with_status(StatusCode::INTERNAL_SERVER_ERROR));
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

    Some(
        build_response(
            settings,
            maybe_file,
            maybe_encoding,
            last_modified,
            mime,
            size,
        )
        .await,
    )
}

async fn build_response(
    settings: &FileRequestOptions,
    maybe_file: Option<File>,
    maybe_encoding: Option<Encoding>,
    last_modified: Option<LastModified>,
    mime: HeaderValue,
    size: u64,
) -> Response<Body> {
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes");

    if let Some(encoding) = maybe_encoding.filter(|encoding| *encoding != Encoding::Identity) {
        builder = builder.header(header::CONTENT_ENCODING, encoding.into_header_value());
    }

    if let Some(last_modified) = last_modified {
        builder = builder.header(header::LAST_MODIFIED, last_modified.0.to_string());
    }

    let maybe_ranges = try_parse_range(settings.range_header.as_deref(), size);
    let maybe_range = match validate_range(&maybe_ranges) {
        Ok(value) => value,
        Err(err) => {
            return builder
                .header(header::CONTENT_RANGE, format!("bytes */{}", size))
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(body_from_message(err.message()))
                .unwrap();
        }
    };

    match maybe_range {
        // Portion of a file
        Some(range) => {
            let content_length = if size == 0 {
                0
            } else {
                range.end() - range.start() + 1
            };

            let body = match maybe_file {
                Some(mut file) => {
                    // Seek to the desired start of the range within the file
                    if let Err(cause) = file.seek(SeekFrom::Start(*range.start())).await {
                        tracing::error!(?cause, "failed to seek to file range start");
                        return response_with_status(StatusCode::INTERNAL_SERVER_ERROR);
                    }

                    Body::new(AsyncReadBody::with_capacity_limited(
                        file,
                        settings.buf_chunk_size,
                        content_length,
                    ))
                }
                None => empty_body(),
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

        // Entire file
        None => {
            let body = match maybe_file {
                Some(file) => {
                    Body::new(AsyncReadBody::with_capacity(file, settings.buf_chunk_size))
                }
                None => empty_body(),
            };

            builder
                .header(header::CONTENT_LENGTH, size.to_string())
                .body(body)
                .unwrap()
        }
    }
}

enum ValidateRangeError {
    TooManyRanges,
    MissingRange,
    InvalidHeader,
}

impl ValidateRangeError {
    fn message(&self) -> &'static str {
        match self {
            ValidateRangeError::TooManyRanges => "cannot serve multipart range requests",
            ValidateRangeError::MissingRange => "no ranges found after parsing range header",
            ValidateRangeError::InvalidHeader => "invalidate range header",
        }
    }
}

fn validate_range(
    maybe_ranges: &Option<Result<Vec<RangeInclusive<u64>>, RangeUnsatisfiableError>>,
) -> Result<Option<&RangeInclusive<u64>>, ValidateRangeError> {
    let ranges = match &maybe_ranges {
        Some(value) => value,
        None => return Ok(None),
    };

    match ranges {
        // Has only one range
        Ok(ranges) => {
            let count = ranges.len();

            // Has more than one range (Only one range is supported)
            if count > 1 {
                return Err(ValidateRangeError::TooManyRanges);
            }

            let range = ranges.first().ok_or(ValidateRangeError::MissingRange)?;
            // Exactly one range
            Ok(Some(range))
        }

        // Has invalid range header
        Err(_) => Err(ValidateRangeError::InvalidHeader),
    }
}
