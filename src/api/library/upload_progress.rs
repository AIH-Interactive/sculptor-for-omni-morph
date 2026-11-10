//! Optional streamed progress; existing JSON upload clients retain their response contract.
use super::*;
use crate::service::resource::UploadProgress;
use axum::body::Body;
use futures_util::stream;
use std::future::Future;

pub(super) fn requested(headers: &HeaderMap) -> bool {
    headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|media| media.trim() == "application/x-ndjson")
        })
}

pub(super) fn response<F, W>(work: F) -> Result<Response>
where
    F: FnOnce(UploadProgress) -> W + Send + 'static,
    W: Future<Output = Result<Value>> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let stages = sender.clone();
        let progress: UploadProgress = std::sync::Arc::new(move |stage| {
            let _ = stages.try_send(json!({"stage": stage}));
        });
        let event = match work(progress).await {
            Ok(result) => json!({"result": result}),
            Err(error) => json!({"error": {
                "code": error.code,
                "status": error.status.as_u16(),
                "retryable": matches!(error.status, StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE)
            }}),
        };
        let _ = sender.send(event).await;
    });
    let events = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| {
            let bytes = serde_json::to_vec(&event).map(|mut bytes| {
                bytes.push(b'\n');
                bytes
            });
            (bytes, receiver)
        })
    });
    let mut response = Response::new(Body::from_stream(events));
    response
        .headers_mut()
        .insert("content-type", "application/x-ndjson".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_real_stages_and_result_in_order() {
        let response = response(|progress| async move {
            progress("VALIDATING");
            progress("REVIEWING");
            Ok(json!({"published": true, "moderation_status": "ACTIVE"}))
        })
        .unwrap_or_else(|_| panic!("progress response creation failed"));
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap_or_else(|_| panic!("progress response creation failed"));
        let events: Vec<Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["stage"], "VALIDATING");
        assert_eq!(events[1]["stage"], "REVIEWING");
        assert_eq!(events[2]["result"]["moderation_status"], "ACTIVE");
    }

    #[tokio::test]
    async fn emits_failure_without_a_success_result() {
        let response = response(|progress| async move {
            progress("VALIDATING");
            Err(ErrorCode::InvalidMessage.into())
        })
        .unwrap_or_else(|_| panic!("progress response creation failed"));
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap_or_else(|_| panic!("progress response creation failed"));
        let events: Vec<Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert!(events[1].get("error").is_some());
        assert!(events[1].get("result").is_none());
    }
}
