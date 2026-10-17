use super::*;
use crate::service::library::{ListQuery, Mutation, OperationInput};
use axum::extract::{Path, Query, State, rejection::JsonRejection};
use serde::Deserialize;

pub async fn capabilities(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    player(&state, &headers)?;
    let config = state.config.read().await;
    json_response(
        json!({"enabled": false, "version": 1, "max_upload_bytes": config.model_library.max_upload_bytes,
        "formats": ["zip", "ysm"], "model_types": ["FIGURA", "YSM_PLAIN", "YSM_ENCRYPTED"],
        "max_page_size": 8, "max_image_bytes": 2097152, "max_image_width": 2048, "max_image_height": 2048,
        "categories": config.model_library.categories, "admin_login_available": !config.model_library.admins.is_empty()}),
    )
}

pub async fn session(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let name = state
        .user_manager
        .get_by_uuid(&actor.uuid)
        .map(|user| user.nickname.clone())
        .ok_or(ErrorCode::Unauthenticated)?;
    json_response(
        json!({"uuid": actor.uuid.to_string(), "name": name, "admin": actor.admin_context}),
    )
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_list(&query, actor)).await?)
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_detail(&id, actor)).await?)
}

pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let categories = state.config.read().await.model_library.categories.clone();
    json_response(
        blocking(move || state.storage.library_patch(actor, &id, &input, &categories)).await?,
    )
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_delete(actor, &id, &input)).await?)
}

async fn change_favorite(
    state: AppState,
    id: String,
    headers: HeaderMap,
    body: std::result::Result<Json<OperationInput>, JsonRejection>,
    enabled: bool,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || state.storage.library_favorite(actor, &id, &input, enabled)).await?,
    )
}

pub async fn favorite(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<OperationInput>, JsonRejection>,
) -> Result<Response> {
    change_favorite(state, id, headers, body, true).await
}

pub async fn unfavorite(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<OperationInput>, JsonRejection>,
) -> Result<Response> {
    change_favorite(state, id, headers, body, false).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    #[serde(default = "first_page")]
    page: u32,
}
fn first_page() -> u32 {
    1
}

pub async fn grants(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_grants(actor, &id, query.page)).await?)
}

async fn change_grant(
    state: AppState,
    id: String,
    player: Uuid,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
    enabled: bool,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || {
            state
                .storage
                .library_grant(actor, &id, player, &input, enabled)
        })
        .await?,
    )
}

pub async fn grant(
    State(state): State<AppState>,
    Path((id, player)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    change_grant(state, id, player, headers, body, true).await
}

pub async fn revoke(
    State(state): State<AppState>,
    Path((id, player)): Path<(String, Uuid)>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    change_grant(state, id, player, headers, body, false).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveQuery {
    name: String,
    model_id: String,
}

pub async fn resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ResolveQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(
        blocking(move || {
            state
                .storage
                .library_resolve(actor, &query.model_id, &query.name)
        })
        .await?,
    )
}

async fn change_block(
    state: AppState,
    id: String,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
    blocked: bool,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_block(actor, &id, &input, blocked)).await?)
}

pub async fn block(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    change_block(state, id, headers, body, true).await
}

pub async fn unblock(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    change_block(state, id, headers, body, false).await
}
