use crate::{api::errors::internal_and_log, ApiError, ApiResult, AppState, AVATARS_VAR};
use std::path::PathBuf;
use tokio::fs;
use uuid::Uuid;

pub fn legacy_path(owner: Uuid, temporary: bool) -> PathBuf {
    let root = PathBuf::from(&*AVATARS_VAR);
    let root = if temporary { root.join("temp") } else { root };
    root.join(format!("{owner}.moon"))
}

pub async fn upload_legacy(state: &AppState, token: &str, bytes: &[u8]) -> ApiResult<()> {
    let user = super::session::authenticate(state, token)?;
    let allowed = state.config.read().await.limitations.can_upload;
    if !state.user_manager.upload_state(user.uuid, allowed) {
        return Err(ApiError::Forbidden);
    }
    // write() completes the full buffer before success, including small packages.
    write_legacy(user.uuid, false, bytes).await
}

pub async fn write_legacy(owner: Uuid, temporary: bool, bytes: &[u8]) -> ApiResult<()> {
    fs::write(legacy_path(owner, temporary), bytes)
        .await
        .map_err(internal_and_log)
}

pub async fn download_legacy(owner: Uuid, requesting_self: bool) -> ApiResult<Vec<u8>> {
    let temporary_path = legacy_path(owner, true);
    let temporary = requesting_self && temporary_path.exists();
    let path = if temporary {
        temporary_path
    } else {
        legacy_path(owner, false)
    };
    let bytes = fs::read(&path).await.map_err(|_| ApiError::NotFound)?;
    if temporary {
        fs::remove_file(path).await.map_err(internal_and_log)?;
    }
    Ok(bytes)
}

pub async fn delete_legacy(state: &AppState, token: &str) -> ApiResult<()> {
    let user = super::session::authenticate(state, token)?;
    fs::remove_file(legacy_path(user.uuid, false))
        .await
        .map_err(internal_and_log)?;
    super::equipment::changed(state, user.uuid).await;
    Ok(())
}
