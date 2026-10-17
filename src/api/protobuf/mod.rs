mod http;
mod resources;
mod subscription;
mod websocket;

#[cfg(test)]
pub(crate) mod tests;

use crate::{
    protocol::{v1::*, wire},
    service::session::ScopedSession,
    AppState,
};
use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Router,
};
use prost::Message;

pub const CONTENT_TYPE: &str = "application/x-protobuf";
pub const SUBPROTOCOL: &str = "omnimorph.cloud.v1";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/protobuf/v1/capabilities", get(http::capabilities))
        .route(
            "/api/protobuf/v1/rpc",
            post(http::rpc).layer(DefaultBodyLimit::max(wire::MAX_ENVELOPE_BYTES)),
        )
        .route(
            "/api/protobuf/v1/resources/uploads/:operation",
            put(resources::upload),
        )
        .route(
            "/api/protobuf/v1/resources/:owner/:asset/:revision",
            get(resources::download),
        )
        .route("/ws/protobuf/v1", get(websocket::upgrade))
}

pub fn response(envelope: Envelope) -> Response {
    if envelope.encoded_len() > wire::MAX_ENVELOPE_BYTES {
        return failure(ErrorCode::LimitExceeded, envelope.request_id, None);
    }
    ([("content-type", CONTENT_TYPE)], envelope.encode_to_vec()).into_response()
}

pub fn envelope(request_id: u64, scope: Option<&ScopedSession>, body: envelope::Body) -> Envelope {
    Envelope {
        major: 1,
        request_id,
        session_id: scope.map(|scope| scope.id.clone()).unwrap_or_default(),
        connection_epoch: scope.map(|scope| scope.epoch).unwrap_or_default(),
        body: Some(body),
    }
}

pub fn error_body(code: ErrorCode, request_id: u64) -> envelope::Body {
    envelope::Body::Error(Error {
        code: code as i32,
        retryable: matches!(
            code,
            ErrorCode::LimitExceeded | ErrorCode::Offline | ErrorCode::Internal
        ),
        localization_key: format!(
            "backend.protobuf.error.{}",
            code.as_str_name().to_ascii_lowercase()
        ),
        request_id,
    })
}

pub fn failure(code: ErrorCode, request_id: u64, scope: Option<&ScopedSession>) -> Response {
    let status = match code {
        ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::LimitExceeded => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    let mut result = response(envelope(request_id, scope, error_body(code, request_id)));
    *result.status_mut() = status;
    result
}

pub fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    epoch: u64,
) -> Result<ScopedSession, ErrorCode> {
    let bearer = headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .ok_or(ErrorCode::Unauthenticated)?;
    if bearer != id {
        return Err(ErrorCode::Unauthenticated);
    }
    state.protocol_sessions.authorize(state, id, epoch)
}

pub fn file_scope(state: &AppState, headers: &HeaderMap) -> Result<ScopedSession, ErrorCode> {
    let id = headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .ok_or(ErrorCode::Unauthenticated)?;
    let epoch = headers
        .get("x-omnimorph-epoch")
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.parse::<u64>().ok())
        .ok_or(ErrorCode::Unauthenticated)?;
    authorize(state, headers, id, epoch)
}

pub async fn hello_ack(state: &AppState) -> HelloAck {
    HelloAck {
        selected_major: 1,
        instance_id: state.protocol_sessions.instance_id.clone(),
        capabilities: vec![
            Capability::Figura as i32,
            Capability::YsmPlain as i32,
            Capability::YsmEncrypted as i32,
            Capability::YsmRecovery as i32,
        ],
        limits: Some(limits(state).await),
        heartbeat_seconds: 20,
        server_epoch: state.protocol_sessions.server_epoch,
    }
}

pub async fn limits(state: &AppState) -> Limits {
    Limits {
        envelope_bytes: wire::MAX_ENVELOPE_BYTES as u32,
        ysm_state_bytes: wire::MAX_STATE_BYTES as u32,
        upload_bytes: state
            .config
            .read()
            .await
            .limitations
            .max_avatar_size
            .saturating_mul(1000),
        messages_per_second: 32,
        bytes_per_second: 262_144,
        subscriptions: 128,
        queue_bytes: 262_144,
        repeated_entries: wire::MAX_ENTRIES as u32,
        nesting_depth: wire::MAX_DEPTH as u32,
        concurrent_uploads: 2,
        event_count: 64,
        event_ttl_seconds: 30,
        snapshot_ttl_seconds: 300,
        decompressed_bytes: 256 * 1024 * 1024,
    }
}
