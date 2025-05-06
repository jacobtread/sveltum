use http::header::HeaderValue;
use http_range_header::RangeUnsatisfiableError;
use httpdate::HttpDate;
use std::{ops::RangeInclusive, time::SystemTime};

pub struct LastModified(pub HttpDate);

impl From<SystemTime> for LastModified {
    fn from(time: SystemTime) -> Self {
        LastModified(time.into())
    }
}

pub struct IfModifiedSince(HttpDate);

impl IfModifiedSince {
    /// Check if the supplied time means the resource has been modified.
    pub fn is_modified(&self, last_modified: &LastModified) -> bool {
        self.0 < last_modified.0
    }

    /// convert a header value into a IfModifiedSince, invalid values are silently ignored
    pub fn from_header_value(value: &HeaderValue) -> Option<IfModifiedSince> {
        std::str::from_utf8(value.as_bytes())
            .ok()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|time| IfModifiedSince(time.into()))
    }
}

pub struct IfUnmodifiedSince(HttpDate);

impl IfUnmodifiedSince {
    /// Check if the supplied time passes the precondition.
    pub fn precondition_passes(&self, last_modified: &LastModified) -> bool {
        self.0 >= last_modified.0
    }

    /// Convert a header value into a IfModifiedSince, invalid values are silently ignored
    pub fn from_header_value(value: &HeaderValue) -> Option<IfUnmodifiedSince> {
        std::str::from_utf8(value.as_bytes())
            .ok()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|time| IfUnmodifiedSince(time.into()))
    }
}

pub fn try_parse_range(
    maybe_range_ref: Option<&str>,
    file_size: u64,
) -> Option<Result<Vec<RangeInclusive<u64>>, RangeUnsatisfiableError>> {
    maybe_range_ref.map(|header_value| {
        http_range_header::parse_range_header(header_value)
            .and_then(|first_pass| first_pass.validate(file_size))
    })
}
