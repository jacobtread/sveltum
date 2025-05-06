use axum_core::body::Body;
use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::{BodyExt, Empty, Full};

pub fn response_with_status(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(empty_body())
        .unwrap()
}

pub fn body_from_bytes(bytes: Bytes) -> Body {
    let body = Full::from(bytes).map_err(|err| match err {});
    Body::new(body)
}

pub fn body_from_message(message: &'static str) -> Body {
    body_from_bytes(Bytes::from(message))
}

pub fn empty_body() -> Body {
    let body = Empty::new().map_err(|err| match err {});
    Body::new(body)
}
