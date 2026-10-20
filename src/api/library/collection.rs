use super::*;
use crate::service::library::collection::{CollectionQuery, JoinInput, OutfitInput};
use axum::extract::{rejection::JsonRejection, Path, Query, State};

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CollectionQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_collection(actor, &query)).await?)
}

pub async fn join(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<JoinInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || state.storage.library_collection_join(actor, &id, &input)).await?,
    )
}

pub async fn outfit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<OutfitInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || state.storage.library_collection_outfit(actor, &id, &input)).await?,
    )
}
