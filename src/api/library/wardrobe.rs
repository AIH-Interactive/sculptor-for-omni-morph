use super::*;
use crate::storage::library_wardrobe::{WardrobeAction, WardrobeQuery, WardrobeVisibility};
use axum::extract::{rejection::JsonRejection, Path, Query, State};

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WardrobeQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_wardrobe_list(actor, &query)).await?)
}

pub async fn recycle_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WardrobeQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.wardrobe_recycle_list(actor, &query)).await?)
}

pub async fn restore(
    State(state): State<AppState>,
    Path((owner, archive)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    body: std::result::Result<Json<crate::storage::wardrobe_recycle::Restore>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let result = blocking(move || storage.wardrobe_restore(actor, owner, archive, &input)).await?;
    state.subscribes.invalidate_observers();
    crate::service::equipment::changed(&state, owner).await;
    json_response(json!({"operation_id": result.operation_id, "equipment_revision": result.revision}))
}

pub async fn own(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let owner = player(&state, &headers)?;
    json_response(blocking(move || state.storage.library_wardrobe_self(owner)).await?)
}

pub async fn visibility(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<WardrobeVisibility>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let result = blocking(move || storage.library_wardrobe_visibility(owner, &input)).await?;
    state.subscribes.invalidate_observers();
    crate::service::equipment::changed(&state, owner).await;
    json_response(
        json!({"operation_id": result.operation_id, "equipment_revision": result.revision}),
    )
}

async fn change(
    state: AppState,
    owner: Uuid,
    headers: HeaderMap,
    body: std::result::Result<Json<WardrobeAction>, JsonRejection>,
    block: bool,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let result =
        blocking(move || storage.library_wardrobe_action(actor, owner, &input, block)).await?;
    if block {
        state.subscribes.invalidate_observers();
    } else {
        crate::service::equipment::changed(&state, owner).await;
    }
    json_response(
        json!({"operation_id": result.operation_id, "equipment_revision": result.revision}),
    )
}

pub async fn reset(
    State(state): State<AppState>,
    Path(owner): Path<Uuid>,
    headers: HeaderMap,
    body: std::result::Result<Json<WardrobeAction>, JsonRejection>,
) -> Result<Response> {
    change(state, owner, headers, body, false).await
}

pub async fn block(
    State(state): State<AppState>,
    Path(owner): Path<Uuid>,
    headers: HeaderMap,
    body: std::result::Result<Json<WardrobeAction>, JsonRejection>,
) -> Result<Response> {
    change(state, owner, headers, body, true).await
}
