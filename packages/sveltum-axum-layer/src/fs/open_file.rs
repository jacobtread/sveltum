use crate::{
    fs::content_encoding::{Encoding, QValue},
    headers::{IfModifiedSince, IfUnmodifiedSince, LastModified},
};
use http::{HeaderValue, Method, Uri, request::Parts};
use http_range_header::RangeUnsatisfiableError;
use std::{
    ffi::OsStr,
    fs::Metadata,
    io::{self, SeekFrom},
    ops::RangeInclusive,
    path::{Path, PathBuf},
};
use tokio::{fs::File, io::AsyncSeekExt};

pub enum OpenFileOutput {
    FileOpened(Box<FileOpened>),
    Redirect { location: HeaderValue },
    FileNotFound,
    PreconditionFailed,
    NotModified,
    InvalidRedirectUri,
}

pub struct FileOpened {
    pub extent: FileRequestExtent,
    pub chunk_size: usize,
    pub mime_header_value: HeaderValue,
    pub maybe_encoding: Option<Encoding>,
    pub maybe_range: Option<Result<Vec<RangeInclusive<u64>>, RangeUnsatisfiableError>>,
    pub last_modified: Option<LastModified>,
}

pub enum FileRequestExtent {
    Full(File, Metadata),
    Head(Metadata),
}

pub struct OpenFileSettings {
    pub if_unmodified_since: Option<IfUnmodifiedSince>,
    pub if_modified_since: Option<IfModifiedSince>,
    pub negotiated_encodings: Vec<(Encoding, QValue)>,
    pub range_header: Option<String>,
    pub buf_chunk_size: usize,
}

pub async fn open_file(
    settings: &OpenFileSettings,
    mut path_to_file: PathBuf,
    parts: &Parts,
) -> io::Result<OpenFileOutput> {
    let mime = {
        // Might already at this point know a redirect or not found result should be
        // returned which corresponds to a Some(output). Otherwise the path might be
        // modified and proceed to the open file/metadata future.
        if let Some(output) =
            maybe_redirect_or_append_path(&mut path_to_file, &parts.uri, true).await
        {
            return Ok(output);
        }

        mime_guess::from_path(&path_to_file)
            .first_raw()
            .map(HeaderValue::from_static)
            .unwrap_or_else(|| {
                HeaderValue::from_str(mime::APPLICATION_OCTET_STREAM.as_ref()).unwrap()
            })
    };

    if parts.method.eq(&Method::HEAD) {
        let (meta, maybe_encoding) =
            file_metadata_with_fallback(path_to_file, settings.negotiated_encodings.clone())
                .await?;

        let last_modified = meta.modified().ok().map(LastModified::from);
        if let Some(output) = check_modified_headers(
            last_modified.as_ref(),
            settings.if_unmodified_since.as_ref(),
            settings.if_modified_since.as_ref(),
        ) {
            return Ok(output);
        }

        let maybe_range = try_parse_range(settings.range_header.as_deref(), meta.len());

        return Ok(OpenFileOutput::FileOpened(Box::new(FileOpened {
            extent: FileRequestExtent::Head(meta),
            chunk_size: settings.buf_chunk_size,
            mime_header_value: mime,
            maybe_encoding,
            maybe_range,
            last_modified,
        })));
    }

    let (mut file, maybe_encoding) =
        open_file_with_fallback(path_to_file, settings.negotiated_encodings.clone()).await?;
    let meta = file.metadata().await?;
    let last_modified = meta.modified().ok().map(LastModified::from);
    if let Some(output) = check_modified_headers(
        last_modified.as_ref(),
        settings.if_unmodified_since.as_ref(),
        settings.if_modified_since.as_ref(),
    ) {
        return Ok(output);
    }

    let maybe_range = try_parse_range(settings.range_header.as_deref(), meta.len());
    if let Some(Ok(ranges)) = maybe_range.as_ref() {
        // if there is any other amount of ranges than 1 we'll return an
        // unsatisfiable later as there isn't yet support for multipart ranges
        if ranges.len() == 1 {
            file.seek(SeekFrom::Start(*ranges[0].start())).await?;
        }
    }

    Ok(OpenFileOutput::FileOpened(Box::new(FileOpened {
        extent: FileRequestExtent::Full(file, meta),
        chunk_size: settings.buf_chunk_size,
        mime_header_value: mime,
        maybe_encoding,
        maybe_range,
        last_modified,
    })))
}

fn check_modified_headers(
    modified: Option<&LastModified>,
    if_unmodified_since: Option<&IfUnmodifiedSince>,
    if_modified_since: Option<&IfModifiedSince>,
) -> Option<OpenFileOutput> {
    if let Some(since) = if_unmodified_since {
        let precondition = modified
            .as_ref()
            .map(|time| since.precondition_passes(time))
            .unwrap_or(false);

        if !precondition {
            return Some(OpenFileOutput::PreconditionFailed);
        }
    }

    if let Some(since) = if_modified_since {
        let unmodified = modified
            .as_ref()
            .map(|time| !since.is_modified(time))
            // no last_modified means its always modified
            .unwrap_or(false);
        if unmodified {
            return Some(OpenFileOutput::NotModified);
        }
    }

    None
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
pub async fn file_metadata_with_fallback(
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

async fn maybe_redirect_or_append_path(
    path_to_file: &mut PathBuf,
    uri: &Uri,
    append_index_html_on_directories: bool,
) -> Option<OpenFileOutput> {
    if !is_dir(path_to_file).await {
        return None;
    }

    if !append_index_html_on_directories {
        return Some(OpenFileOutput::FileNotFound);
    }

    if uri.path().ends_with('/') {
        path_to_file.push("index.html");
        None
    } else {
        let uri = match append_slash_on_path(uri.clone()) {
            Ok(uri) => uri,
            Err(err) => return Some(err),
        };
        let location = HeaderValue::from_str(&uri.to_string()).unwrap();
        Some(OpenFileOutput::Redirect { location })
    }
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

async fn is_dir(path_to_file: &Path) -> bool {
    tokio::fs::metadata(path_to_file)
        .await
        .is_ok_and(|meta_data| meta_data.is_dir())
}

fn append_slash_on_path(uri: Uri) -> Result<Uri, OpenFileOutput> {
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
        OpenFileOutput::InvalidRedirectUri
    })
}
