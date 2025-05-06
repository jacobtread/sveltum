use std::{fs::Metadata, io, path::PathBuf};

use tokio::fs::File;

use crate::Encoding;

// Modify the `path` to add the provided `file_extension`
fn apply_file_extension(path: &mut PathBuf, file_extension: &'static std::ffi::OsStr) {
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

pub async fn try_open_file(
    path_to_file: PathBuf,
    negotiated_encodings: &[Encoding],
    head: bool,
) -> io::Result<(Option<File>, Metadata, Option<Encoding>)> {
    if head {
        let (meta, maybe_encoding) =
            file_metadata_with_fallback(path_to_file, negotiated_encodings).await?;
        return Ok((None, meta, maybe_encoding));
    }

    let (file, maybe_encoding) =
        open_file_with_fallback(path_to_file, negotiated_encodings).await?;
    let meta = file.metadata().await?;
    Ok((Some(file), meta, maybe_encoding))
}

// Attempts to open the file with any of the possible negotiated_encodings in the
// preferred order. If none of the negotiated_encodings have a corresponding precompressed
// file the uncompressed file is used as a fallback.
pub async fn open_file_with_fallback(
    mut path: PathBuf,
    negotiated_encodings: &[Encoding],
) -> io::Result<(File, Option<Encoding>)> {
    let mut encoding_iter = negotiated_encodings.iter();

    let (file, encoding) = loop {
        // Get the preferred encoding among the negotiated ones.
        let encoding = encoding_iter.next().copied();

        if let Some(file_extension) = encoding.and_then(|encoding| encoding.to_file_extension()) {
            // Apply the encoding file extension
            apply_file_extension(&mut path, file_extension);
        }

        match File::open(&path).await {
            Ok(file) => break (file, encoding),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound && encoding.is_some() {
                    // Remove the extension corresponding to a precompressed file (.gz, .br, .zz)
                    // to reset the path before the next iteration.
                    path.set_extension("");
                    continue;
                }

                return Err(err);
            }
        }
    };

    Ok((file, encoding))
}

// Attempts to get the file metadata with any of the possible negotiated_encodings in the
// preferred order. If none of the negotiated_encodings have a corresponding precompressed
// file the uncompressed file is used as a fallback.
pub async fn file_metadata_with_fallback(
    mut path: PathBuf,
    negotiated_encodings: &[Encoding],
) -> io::Result<(Metadata, Option<Encoding>)> {
    let mut encoding_iter = negotiated_encodings.iter();

    let (file, encoding) = loop {
        // Get the preferred encoding among the negotiated ones.
        let encoding = encoding_iter.next().copied();

        if let Some(file_extension) = encoding.and_then(|encoding| encoding.to_file_extension()) {
            // Apply the encoding file extension
            apply_file_extension(&mut path, file_extension);
        }

        match tokio::fs::metadata(&path).await {
            Ok(file) => break (file, encoding),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound && encoding.is_some() {
                    // Remove the extension corresponding to a precompressed file (.gz, .br, .zz)
                    // to reset the path before the next iteration.
                    path.set_extension("");
                    continue;
                }

                return Err(err);
            }
        }
    };

    Ok((file, encoding))
}
