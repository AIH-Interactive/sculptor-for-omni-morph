use axum::{body::Bytes, extract::{Path, State}};
use uuid::Uuid;

use crate::{api::figura::profile::send_event, auth::Token, ApiResult, AppState};

pub async fn upload_avatar(
    Path(uuid): Path<Uuid>,
    Token(token): Token,
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<&'static str> {
    let request_data = body;

    state.config.read().await.clone().verify_token(&token)?;

    tracing::info!(
        "trying to upload the avatar for {}",
        uuid,
    );

    crate::service::resource::write_legacy(uuid, false, &request_data).await?;
    crate::service::resource::ensure_legacy_import(&state, uuid, true)
        .await
        .map_err(crate::service::resource::legacy_error)?;
    send_event(&state, &uuid).await;

    Ok("ok")
}

pub async fn delete_avatar(
    Path(uuid): Path<Uuid>,
    Token(token): Token,
    State(state): State<AppState>
) -> ApiResult<&'static str> {
    state.config.read().await.clone().verify_token(&token)?;

    tracing::info!(
        "trying to delete the avatar for {}",
        uuid,
    );

    crate::service::resource::delete_legacy_owner(&state, uuid).await?;

    Ok("ok")
}
