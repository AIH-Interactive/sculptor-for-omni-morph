use super::*;
use crate::service::{
    library::{
        Mutation, OperationInput,
        uploads::{CommitContext, UploadInput},
    },
    resource,
};
use axum::{
    body::Body,
    extract::{Path, State, rejection::JsonRejection},
};

pub async fn reserve(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<UploadInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let filename = input.source_filename.to_ascii_lowercase();
    if !(filename.ends_with(".zip") || filename.ends_with(".ysm")) {
        return Err(LibraryError::unsupported());
    }
    let config = state.config.read().await.model_library.clone();
    if input.byte_length > config.max_upload_bytes {
        return Err(LibraryError::too_large());
    }
    let name = state
        .user_manager
        .get_by_uuid(&actor.uuid)
        .map(|user| user.nickname.clone())
        .ok_or(ErrorCode::Unauthenticated)?;
    let storage = state.storage.clone();
    let reservation =
        blocking(move || storage.library_reserve(actor, &name, &input, &config.categories)).await?;
    let storage = state.storage.clone();
    let operation = reservation.request.operation_id.clone();
    let policy_revision = blocking(move || {
        let pending = storage.library_upload_pending(actor, &operation)?;
        storage.library_read(|database| {
            crate::service::library::model(database, &pending.model_id, actor, true)
                .map(|item| item.summary.policy_revision)
        })
    })
    .await?;
    let descriptor = reservation.descriptor(actor.uuid)?;
    let grant = resource::grant_library_upload(&state, actor.uuid, &descriptor).await?;
    json_response(
        json!({"model_id": reservation.model_id, "operation_id": grant.operation_id,
        "relative_path": format!("/api/library/v1/uploads/{}/body", grant.operation_id),
        "expires_at": grant.expires_unix_seconds, "max_bytes": grant.max_bytes,
        "policy_revision": policy_revision}),
    )
}

pub async fn upload(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let operation_id = resource::operation_id(&operation)?;
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err(LibraryError::unsupported());
    }
    let storage = state.storage.clone();
    let reservation = blocking(move || storage.library_upload_pending(actor, &operation)).await?;
    if reservation.request.byte_length > state.config.read().await.model_library.max_upload_bytes {
        return Err(LibraryError::too_large());
    }
    let mut upload = resource::begin_upload(&state, actor.uuid, operation_id)?;
    resource::receive_upload(&mut upload, body, || {
        // Revalidate the existing player session for each chunk without database work.
        let current =
            super::player_identity(&state, &headers).map_err(|_| ErrorCode::Unauthenticated)?;
        if current != actor.uuid {
            return Err(ErrorCode::Unauthenticated);
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn commit(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let lookup = operation.clone();
    let reservation = blocking(move || storage.library_reservation(actor.uuid, &lookup)).await?;
    if reservation.request.byte_length > state.config.read().await.model_library.max_upload_bytes {
        return Err(LibraryError::too_large());
    }
    let model_id = reservation.model_id.clone();
    let request = crate::protocol::v1::CommitUpload {
        operation_id: operation,
        raw_sha256: reservation
            .descriptor(actor.uuid)?
            .resource
            .ok_or(ErrorCode::Internal)?
            .raw_sha256,
        raw_length: reservation.request.byte_length,
    };
    resource::commit_library_upload(
        &state,
        actor.uuid,
        &request,
        CommitContext {
            actor,
            reservation,
            input,
        },
    )
    .await?;
    json_response(json!({"model_id": model_id, "published": true}))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<OperationInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let lookup = operation.clone();
    let result = blocking(move || storage.library_cancel(actor, &lookup, &input)).await?;
    resource::cancel_upload(&state, actor.uuid, resource::operation_id(&operation)?)?;
    json_response(result)
}
