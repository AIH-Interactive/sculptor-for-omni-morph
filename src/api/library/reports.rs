use super::*;
use crate::service::library::reports::{ReportInput, ReportQuery};
use crate::service::library::Mutation;
use axum::extract::{rejection::JsonRejection, Path, Query, State};

pub async fn submit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ReportInput>, JsonRejection>,
) -> Result<Response> {
    // Reporting never inherits an administrator's broader catalog visibility.
    let uuid = player(&state, &headers)?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || {
            state.storage.library_report(
                Actor {
                    uuid,
                    admin_context: false,
                    granted: false,
                },
                &id,
                &input,
            )
        })
        .await?,
    )
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ReportQuery>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    json_response(blocking(move || state.storage.library_reports(actor, &query)).await?)
}

pub async fn resolve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_resolve_report(actor, &id, &input)).await?)
}
