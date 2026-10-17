//! Test-only capture of actual HTTP request streams, without buffering file bodies.
use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use futures_util::StreamExt;
use ring::digest::{Context, SHA256};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<Record>>>);

struct Record {
    path: String,
    method: String,
    content_type: String,
    body: Arc<Mutex<(usize, Context)>>,
}

pub async fn requests(State(capture): State<Capture>, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let digest = Arc::new(Mutex::new((0, Context::new(&SHA256))));
    capture.0.lock().unwrap().push(Record {
        path: parts.uri.path().into(),
        method: parts.method.to_string(),
        content_type: parts
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .into(),
        body: digest.clone(),
    });
    let stream = body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            let mut digest = digest.lock().unwrap();
            digest.0 += bytes.len();
            digest.1.update(bytes);
        }
        chunk
    });
    next.run(Request::from_parts(parts, Body::from_stream(stream)))
        .await
}

impl Capture {
    pub fn assert_raw_upload(&self, bytes: &[u8], legacy: bool) {
        let expected = crate::storage::sha256(bytes);
        let records = self.0.lock().unwrap();
        assert!(
            records.iter().any(|record| {
                let body = record.body.lock().unwrap();
                record.method == "PUT"
                    && if legacy {
                        record.path == "/api/avatar"
                    } else {
                        record
                            .path
                            .starts_with("/api/protobuf/v1/resources/uploads/")
                    }
                    && (legacy || record.content_type == "application/octet-stream")
                    && body.0 == bytes.len()
                    && body.1.clone().finish().as_ref() == expected
            }),
            "No raw HTTP request with the expected file digest was captured"
        );
        for record in records
            .iter()
            .filter(|record| record.path == "/api/protobuf/v1/rpc")
        {
            let body = record.body.lock().unwrap();
            assert_eq!(record.content_type, "application/x-protobuf");
            assert!(body.0 <= super::wire::MAX_ENVELOPE_BYTES + 1); // Includes the explicit rejection probe.
            assert_ne!(
                body.1.clone().finish().as_ref(),
                expected,
                "File bytes reached the RPC route"
            );
        }
    }
}
