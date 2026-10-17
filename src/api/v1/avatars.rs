use axum::{body::Bytes, extract::{Path, State}};
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

use crate::{api::figura::profile::send_event, auth::Token, ApiResult, AppState, AVATARS_VAR};

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

    let avatar_file = format!("{}/{}.moon", *AVATARS_VAR, &uuid);
    match fs::remove_file(avatar_file).await {
        Ok(_) => {},
        Err(_) => {
            warn!("avatar doesn't exist");
            return Err(crate::ApiError::NotFound)
        }
    };
    send_event(&state, &uuid).await;

    Ok("ok")
}
