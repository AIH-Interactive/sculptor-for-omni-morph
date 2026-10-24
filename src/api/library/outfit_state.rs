use super::*;
use crate::{
    api::figura::SessionMessage, service::event::CloudEvent, storage::outfit_state::InstanceState,
};
use axum::extract::{rejection::JsonRejection, Path, State};

pub async fn read(
    State(state): State<AppState>,
    Path((owner, instance)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Response> {
    let viewer = player(&state, &headers)?;
    let origin = origin(&headers)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let result = blocking(move || {
        state
            .storage
            .outfit_instance_state(owner, viewer, &origin, instance, &limits, None)
    })
    .await?;
    json_response(serde_json::to_value(result).map_err(|_| ErrorCode::Internal)?)
}

pub async fn write(
    State(state): State<AppState>,
    Path(instance): Path<Uuid>,
    headers: HeaderMap,
    body: std::result::Result<Json<InstanceState>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let origin = origin(&headers)?;
    let input = parse_json(body)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let storage = state.storage.clone();
    let result = blocking(move || {
        storage.outfit_instance_state(owner, owner, &origin, instance, &limits, Some(&input))
    })
    .await?;
    let event = CloudEvent::OutfitStateChanged { owner, instance };
    let _ = state.subscribes.publisher(owner).send(event.clone());
    if let Some(sessions) = state.session.get(&owner) {
        let _ = sessions.try_send(SessionMessage::Event(event));
    }
    json_response(serde_json::to_value(result).map_err(|_| ErrorCode::Internal)?)
}
