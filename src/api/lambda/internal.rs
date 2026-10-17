use axum::{async_trait, body::Bytes, extract::{Path, State}};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use tracing::{debug, trace};
use uuid::Uuid;

use crate::{ApiError, ApiResult, AppState};
use super::super::figura::websocket::S2CMessage;
use super::super::figura::websocket::SessionMessage;

pub async fn temp_avatar(
    Path(uuid): Path<Uuid>,
    Host(host): Host,
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<String> {
    internal_or_error(host).await?;
    let request_data = body;

    if let Some(user_info) = state.user_manager.get_by_uuid(&uuid) {
        tracing::info!(
            "internal api trying upload temp avatar for {} ({})",
            user_info.uuid,
            user_info.nickname
        );
        state.user_manager.put_request_temp_state(uuid, false);
        crate::service::resource::write_legacy(&state, user_info.uuid, true, &request_data).await?;
    }
    Ok("ok".to_string())
}

pub async fn upload_avatar(
    Path(uuid): Path<Uuid>,
    Host(host): Host,
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<String> {
    internal_or_error(host).await?;
    let request_data = body;

    if let Some(user_info) = state.user_manager.get_by_uuid(&uuid) {
        tracing::info!(
            "internal api trying upload avatar for {} ({})",
            user_info.uuid,
            user_info.nickname
        );
        crate::service::resource::write_legacy(&state, user_info.uuid, false, &request_data)
            .await?;
        crate::service::resource::ensure_legacy_import(&state, user_info.uuid, true)
            .await
            .map_err(crate::service::resource::legacy_error)?;
    }
    Ok("ok".to_string())
}

pub async fn delete_avatar(
    Path(uuid): Path<Uuid>,
    Host(host): Host,
    State(state): State<AppState>
) -> ApiResult<String> {
    internal_or_error(host).await?;
    if let Some(user_info) = state.user_manager.get_by_uuid(&uuid) {
        tracing::info!(
            "internal api trying to delete avatar for {} ({})",
            user_info.uuid,
            user_info.nickname
        );
        crate::service::resource::delete_legacy_owner(&state, user_info.uuid).await?;
    }
    Ok("ok".to_string())
}

pub async fn user_event(
    Path(uuid): Path<Uuid>,
    Host(host): Host,
    State(state): State<AppState>,
) -> ApiResult<String> {
    internal_or_error(host).await?;
    tracing::info!("internal api request update avatar for user {}", uuid);
    if let Some(session) = state.session.get(&uuid) {
        if session.send(SessionMessage::Ping(S2CMessage::Event(uuid).into())).await.is_err() {
            debug!("[WebSocket] Failed to send Event! WS doesn't connected? UUID: {uuid}")
        };
    } else {
        debug!("[WebSocket] Failed to send Event! Can't find UUID: {uuid}")
    };
    Ok("ok".to_string())
}

pub async fn user_upload_state(
    Path((uuid, us)): Path<(Uuid, bool)>,
    Host(host): Host,
    State(state): State<AppState>,
) -> ApiResult<String> {
    internal_or_error(host).await?;
    if let Some(user_info) = state.user_manager.get_by_uuid(&uuid) {
        tracing::info!(
            "internal api trying to update upload state to {} for {} ({})",
            us,
            user_info.uuid,
            user_info.nickname
        );
        state
            .user_manager
            .put_upload_state(uuid, us)
            .map_err(crate::service::resource::legacy_error)?;
    }
    Ok("ok".to_string())
}
#[derive(PartialEq, Debug)]
pub struct Host(pub String);
#[async_trait]
impl<S> FromRequestParts<S> for Host
where
    S: Send + Sync,
{
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let host = parts
            .headers
            .get("host")
            .and_then(|value| value.to_str().ok());
        trace!(present = host.is_some(), "Internal host header inspected");
        match host {
            Some(host) => Ok(Self(host.to_string())),
            None => Err(StatusCode::NOT_FOUND),
        }
    }
}
pub async fn check_internal(
    host: Option<Host>,
) -> ApiResult<&'static str> {
    debug!("Checking internal actuality...");
    match host {
        Some(host) => {
            let host_value = host.0;
            let target = String::from("lambda");
            if host_value == target {
                Ok("ok")
            } else {
                Err(ApiError::Forbidden)
            }
        },
        None => Err(ApiError::NotFound),
    }
}
pub async fn internal_or_error(
    host: String
) -> ApiResult<()> {
    let lambda = String::from("lambda");
    if lambda == host {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}
