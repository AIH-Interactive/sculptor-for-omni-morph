use super::*;
use crate::storage::outfit::PresetInput;
use crate::service::library;
use axum::extract::{rejection::JsonRejection, Path, State};

pub(super) const APPLY_JSON_MAX_BYTES: usize = crate::service::outfit::MAX_JSON_BYTES + 4096;

pub async fn apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<crate::storage::outfit_apply::ApplyInput>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let origin = origin(&headers)?;
    let input = parse_json(body)?;
    let operation = input.operation_id;
    let limits = state.config.read().await.outfit.limits.clone();
    let storage = state.storage.clone();
    let (outcome, current) = blocking(move || {
        let actor = crate::service::library_policy::Actor {
            uuid: owner,
            admin_context: false,
            granted: false,
        };
        let outcome = match storage.outfit_apply_with_status(actor, &origin, &input, &limits) {
            Ok(outcome) => Some(outcome),
            Err(ErrorCode::Conflict) => None,
            Err(error) => return Err(error),
        };
        Ok((outcome, storage.outfit_current(owner)?))
    })
    .await?;
    if let Some(outcome) = outcome {
        if outcome.changed {
            crate::service::equipment::changed(&state, owner).await;
        }
        return json_response_limited(
            json!({"schema_version": 1, "operation_id": operation,
            "applied": outcome.applied, "current_revision": current.revision}),
            APPLY_JSON_MAX_BYTES,
        );
    }
    let request_id = library::random_id();
    let mut response = json_response_limited(
        json!({
            "code": "CONFLICT", "localization_key": "library.error.conflict", "request_id": request_id,
            "retryable": false, "current_revision": current.revision, "configuration": current,
        }),
        APPLY_JSON_MAX_BYTES,
    )?;
    *response.status_mut() = StatusCode::CONFLICT;
    response.headers_mut().insert(
        "x-request-id",
        request_id.parse().map_err(|_| ErrorCode::Internal)?,
    );
    Ok(response)
}

pub async fn bind_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<crate::storage::outfit_resources::LocalBinding>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let binding = blocking(move || state.storage.outfit_bind_local(owner, &input, &limits)).await?;
    json_response(serde_json::to_value(binding).map_err(|_| ErrorCode::Internal)?)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveLocal {
    reference: crate::service::outfit::ResourceRef,
    resource_kind: crate::service::outfit::ResourceKind,
}

pub async fn resolve_local(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<ResolveLocal>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let resource = blocking(move || {
        state.storage.outfit_resolve_local(
            owner,
            &input.reference,
            input.resource_kind,
            &limits,
        )
    })
    .await?;
    json_response(json!({"resource": resource}))
}

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
