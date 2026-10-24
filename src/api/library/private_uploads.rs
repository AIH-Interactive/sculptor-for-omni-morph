//! Owner-private source uploads; no catalog entry, membership or equipment mutation.
use super::*;
use crate::{service::resource, storage::outfit_uploads::PrivateUpload};
use axum::{
    body::Body,
    extract::{Path, State, rejection::JsonRejection},
};

async fn committed(state: &AppState, owner: Uuid, input: &PrivateUpload) -> Result<Value> {
    let storage = state.storage.clone();
    let input = input.clone();
    let limits = state.config.read().await.outfit.limits.clone();
    let binding = blocking(move || {
        storage.library_read(|database| {
            crate::storage::outfit_resources::resolve_local(
                database,
                owner,
                &input.reference,
                input.resource_kind,
                &limits,
            )
        })
    })
    .await?;
    serde_json::to_value(binding).map_err(|_| ErrorCode::Internal.into())
}

pub async fn reserve(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<PrivateUpload>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let config = state.config.read().await;
    if input.byte_length > config.limitations.max_avatar_size.saturating_mul(1000) {
        return Err(LibraryError::too_large());
    }
    if !state
        .user_manager
        .upload_state(owner, config.limitations.can_upload)
    {
        return Err(ErrorCode::Forbidden.into());
    }
    let limits = config.outfit.limits.clone();
    let max_assets = config.limitations.max_avatars;
    drop(config);
    let storage = state.storage.clone();
    let reserved =
        blocking(move || storage.outfit_reserve_upload(owner, &input, &limits, max_assets)).await?;
    let storage = state.storage.clone();
    let operation = reserved.operation_id;
    let limits = state.config.read().await.outfit.limits.clone();
    if blocking(move || storage.outfit_completed_upload(owner, operation, &limits))
        .await?
        .is_some()
    {
        return json_response(json!({"operation_id": operation, "completed": true,
            "resource": committed(&state, owner, &reserved).await?}));
    }
    let grant = resource::grant_upload(&state, owner, &reserved.request(owner)?).await?;
    json_response(json!({"operation_id": operation, "completed": false,
        "relative_path": format!("/api/library/v1/outfits/resources/uploads/{operation}/body"),
        "expires_at": grant.expires_unix_seconds, "max_bytes": grant.max_bytes}))
}

pub async fn upload(
    State(state): State<AppState>,
    Path(operation): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err(LibraryError::unsupported());
    }
    let storage = state.storage.clone();
    let reserved = blocking(move || storage.outfit_upload_reservation(owner, operation)).await?;
    let config = state.config.read().await;
    reserved.validate(owner, &config.outfit.limits)?;
    if reserved.byte_length > config.limitations.max_avatar_size.saturating_mul(1000) {
        return Err(LibraryError::too_large());
    }
    drop(config);
    let mut upload = resource::begin_upload(&state, owner, operation)?;
    resource::receive_upload(&mut upload, body, || {
        if super::player_identity(&state, &headers).map_err(|_| ErrorCode::Unauthenticated)?
            != owner
        {
            return Err(ErrorCode::Unauthenticated);
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn commit(
    State(state): State<AppState>,
    Path(operation): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let storage = state.storage.clone();
    let reserved = blocking(move || storage.outfit_upload_reservation(owner, operation)).await?;
    let config = state.config.read().await;
    reserved.validate(owner, &config.outfit.limits)?;
    if reserved.byte_length > config.limitations.max_avatar_size.saturating_mul(1000) {
        return Err(LibraryError::too_large());
    }
    drop(config);
    let descriptor = reserved
        .request(owner)?
        .resource
        .ok_or(ErrorCode::Internal)?;
    resource::commit_upload(
        &state,
        owner,
        &crate::protocol::v1::CommitUpload {
            operation_id: operation.to_string(),
            raw_sha256: descriptor.raw_sha256,
            raw_length: descriptor.raw_length,
        },
    )
    .await?;
    json_response(committed(&state, owner, &reserved).await?)
}
