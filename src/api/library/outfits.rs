use super::*;
use crate::storage::outfit::PresetInput;
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
    let preset = blocking(move || state.storage.outfit_save_preset(owner, &input)).await?;
    json_response(serde_json::to_value(preset).map_err(|_| ErrorCode::Internal)?)
}
