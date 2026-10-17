use std::ops::Add;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use axum::{
    body::Bytes, extract::{Path, State}, Json
};
use tracing::debug;
use serde_json::{json, Value};
use tokio::fs;
use uuid::Uuid;

use crate::{
    auth::Token, utils::{calculate_file_sha256, format_uuid},
    ApiError, ApiResult, AppState, AVATARS_VAR
};

pub fn is_requesting_self(uuid: Uuid, state: &AppState, token: &String) -> bool {
    return if let Some(user_info) = state.user_manager.get(token) {
        let user_uuid = user_info.uuid;
        uuid.eq(&user_uuid)
    } else { false };
}

pub async fn user_info(
    Path(uuid): Path<Uuid>,
    Token(token): Token,
    State(state): State<AppState>,
) -> ApiResult<Json<Value>> {
    crate::service::session::authenticate(&state, &token)?;
    tracing::info!("Receiving profile information for {}", uuid);

    let formatted_uuid = format_uuid(&uuid);

    let request_temp_state = state.user_manager.request_temp_state(uuid, false);
    let request_self_avatar = is_requesting_self(uuid, &state, &token);
    let temp_avatar_file = format!("{}/temp/{}.moon", *AVATARS_VAR, formatted_uuid);
    let path = PathBuf::from(&temp_avatar_file);
    let outdated = if path.exists() {
        let meta = path.metadata().unwrap();
        let last_modified = meta.modified().unwrap();
        SystemTime::now() > last_modified.add(Duration::from_secs(60))
    } else { false };
    let use_temp = !request_temp_state && request_self_avatar && !outdated;
    let avatar_file = if use_temp {
        tracing::info!("Profile {} is self requesting and it is temp", uuid);
        state.user_manager.put_request_temp_state(uuid, true);
        temp_avatar_file
    } else {
        format!("{}/{}.moon", *AVATARS_VAR, formatted_uuid)
    };

    let userinfo = if let Some(info) = state.user_manager.get_by_uuid(&uuid) { info.clone() } else {
        return Err(ApiError::BadRequest) // NOTE: Not Found (404) shows badge
    };

    let mut user_info_response = json!({
        "uuid": &formatted_uuid,
        "rank": userinfo.rank,
        "equipped": [],
        "lastUsed": userinfo.last_used,
        "equippedBadges": {
            "special": [0,0,0,0,0,0],
            "pride": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]
        },
        "version": userinfo.version,
        "banned": userinfo.banned
    });

    if let Some(settings) = state.config.read().await.advanced_users.clone().get(&uuid) {
        let badges = user_info_response
            .get_mut("equippedBadges")
            .and_then(Value::as_object_mut)
            .unwrap();
        badges.append(
            json!({
                "special": settings.special,
                "pride": settings.pride
            })
            .as_object_mut()
            .unwrap(),
        )
    }

    let hash = if use_temp {
        if fs::metadata(&avatar_file).await.is_ok() {
            calculate_file_sha256(&avatar_file).ok()
        } else {
            None
        }
    } else {
        crate::service::resource::ensure_legacy_import(&state, uuid, false).await
            .map_err(crate::service::resource::legacy_error)?;
        state.storage.equipped(uuid).map_err(crate::service::resource::legacy_error)?
            .figura_compatibility.map(|avatar| avatar.legacy_figura_hash)
    };
    if let Some(hash) = hash {
        user_info_response["equipped"].as_array_mut().unwrap().push(json!({
            "id": "avatar", "owner": &formatted_uuid, "hash": hash
        }));
    }
    Ok(Json(user_info_response))
}

pub async fn download_avatar(
    Path(uuid): Path<Uuid>,
    Token(token): Token,
    State(state): State<AppState>
) -> ApiResult<Vec<u8>> {
    crate::service::session::authenticate(&state, &token)?;
    let str_uuid = format_uuid(&uuid);
    tracing::info!("Requesting an avatar: {}", str_uuid);

    crate::service::resource::download_legacy(&state, uuid, is_requesting_self(uuid, &state, &token)).await
}

pub async fn upload_avatar(
    Token(token): Token,
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<String> {
    crate::service::resource::upload_legacy(&state, &token, &body).await?;
    Ok("ok".to_string())
}

pub async fn equip_avatar(Token(token): Token, State(state): State<AppState>) -> ApiResult<&'static str> {
    debug!("[API] S2C : Equip");
    let uuid = crate::service::session::authenticate(&state, &token)?.uuid;
    send_event(&state, &uuid).await;
    Ok("ok")
}

pub async fn delete_avatar(Token(token): Token, State(state): State<AppState>) -> ApiResult<String> {
    crate::service::resource::delete_legacy(&state, &token).await?;
    Ok("ok".to_string())
}

pub async fn send_event(state: &AppState, uuid: &Uuid) {
    crate::service::equipment::changed(state, *uuid).await;
}
