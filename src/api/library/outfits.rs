use super::*;
use crate::storage::outfit::PresetInput;
use crate::service::library;
use axum::extract::{rejection::JsonRejection, Path, State};

pub async fn current(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let config = blocking(move || state.storage.outfit_current(owner)).await?;
    json_response(serde_json::to_value(config).map_err(|_| ErrorCode::Internal)?)
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let items = blocking(move || state.storage.outfit_presets(owner)).await?;
    json_response(json!({"items": items}))
}

pub async fn read(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let preset = blocking(move || state.storage.outfit_preset(owner, id)).await?;
    json_response(serde_json::to_value(preset).map_err(|_| ErrorCode::Internal)?)
}

pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<PresetInput>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let preset = blocking(move || state.storage.outfit_save_preset(owner, &input, &limits)).await?;
    json_response(serde_json::to_value(preset).map_err(|_| ErrorCode::Internal)?)
}

pub async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<library::outfit_uploads::OutfitUploadInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let origin = origin(&headers)?;
    let input = parse_json(body)?;
    let name = state
        .user_manager
        .get_by_uuid(&actor.uuid)
        .map(|user| user.nickname.clone())
        .ok_or(ErrorCode::Unauthenticated)?;
    let config = state.config.read().await;
    let limits = config.outfit.limits.clone();
    let categories = config.model_library.effective_categories();
    drop(config);
    let storage = state.storage.clone();
    let result = blocking(move || {
        storage.library_publish_outfit(actor, &name, &origin, &input, &categories, &limits)
    })
    .await?;
    json_response(result)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedQuery {
    pub revision: u64,
}

pub async fn published(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<PublishedQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let result = blocking(move || {
        state
            .storage
            .library_read_outfit(actor, &id.to_string(), query.revision)
    })
    .await?;
    json_response(result)
}
