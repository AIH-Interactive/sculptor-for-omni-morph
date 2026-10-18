use super::*;
use crate::service::library::{
    appeals::{AppealInput, AppealQuery, ResolveAppeal},
    Mutation,
};
use axum::extract::{rejection::JsonRejection, Path, Query, State};

pub async fn targets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AppealQuery>,
) -> Result<Response> {
    let uuid = player(&state, &headers)?;
    json_response(
        blocking(move || {
            state.storage.library_appeal_targets(
                Actor {
                    uuid,
                    admin_context: false,
                    granted: false,
                },
                query.page,
            )
        })
        .await?,
    )
}

pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<AppealInput>, JsonRejection>,
) -> Result<Response> {
    let uuid = player(&state, &headers)?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || {
            state.storage.library_appeal(
                Actor {
                    uuid,
                    admin_context: false,
                    granted: false,
                },
                &input,
            )
        })
        .await?,
    )
}

pub async fn mine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AppealQuery>,
) -> Result<Response> {
    let uuid = player(&state, &headers)?;
    json_response(
        blocking(move || {
            state.storage.library_appeals(
                Actor {
                    uuid,
                    admin_context: false,
                    granted: false,
                },
                &query,
                false,
            )
        })
        .await?,
    )
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AppealQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_appeals(actor, &query, true)).await?)
}

pub async fn resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ResolveAppeal>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_resolve_appeal(actor, &id, &input)).await?)
}

pub async fn unblock(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    let storage = state.storage.clone();
    let result = blocking(move || storage.library_unblock_appeal(actor, &id, &input)).await?;
    state.subscribes.invalidate_observers();
    json_response(result)
}
