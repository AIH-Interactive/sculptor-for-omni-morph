//! Authenticated PSF preparation. Publication itself remains the existing upload flow.
use super::*;
use crate::service::psf::{Error, Purpose, MAX_SOURCE};
use axum::{
    body::{to_bytes, Body},
    extract::State,
};

pub(super) fn failure(error: Error) -> LibraryError {
    let (status, code) = match error {
        Error::KeyUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "PSF_KEY_UNAVAILABLE"),
        Error::KeyLength => (StatusCode::SERVICE_UNAVAILABLE, "PSF_KEY_LENGTH"),
        Error::InvalidPackage => (StatusCode::BAD_REQUEST, "PSF_INVALID_PACKAGE"),
        Error::Forbidden => (StatusCode::FORBIDDEN, "PSF_GRANT_FORBIDDEN"),
    };
    LibraryError { status, code }
}

pub(crate) fn session_binding(headers: &HeaderMap) -> Result<String> {
    let token = headers
        .get("authorization")
        .ok_or(ErrorCode::Unauthenticated)?;
    let epoch = headers
        .get("x-omnimorph-epoch")
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    let mode = headers
        .get("x-omnimorph-auth-mode")
        .map(|value| value.as_bytes())
        .unwrap_or(b"protobuf");
    Ok(crate::service::psf::digest(
        &[token.as_bytes(), b"\0", epoch, b"\0", mode].concat(),
    ))
}

pub(crate) fn publication(
    state: &AppState,
    headers: &HeaderMap,
    owner: Uuid,
    content: &[u8],
) -> Result<crate::service::psf::Publication> {
    let token = headers
        .get("x-omnimorph-psf-grant")
        .ok_or_else(|| failure(Error::Forbidden))?
        .to_str()
        .map_err(|_| ErrorCode::InvalidMessage)?
        .to_owned();
    let session = session_binding(headers)?;
    let current_state = state.clone();
    let current_headers = headers.clone();
    let proof = crate::service::psf::Publication::new(
        token,
        session,
        std::sync::Arc::new(move || {
            super::player_identity(&current_state, &current_headers)
                .is_ok_and(|current| current == owner)
        }),
    );
    proof
        .verify(
            &state.psf_keys,
            owner,
            content,
            crate::storage::now_seconds(),
        )
        .map_err(failure)?;
    Ok(proof)
}

pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let allowed = state
        .user_manager
        .get_by_uuid(&actor.uuid)
        .map(|user| !user.banned && !user.nickname.is_empty())
        .unwrap_or(false);
    if !allowed {
        return Err(ErrorCode::Forbidden.into());
    }
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err(LibraryError::unsupported());
    }
    let purpose = match headers
        .get("x-omnimorph-psf-purpose")
        .and_then(|value| value.to_str().ok())
    {
        Some("PUBLICATION") => Purpose::Publication,
        Some("FITTING") => Purpose::Fitting,
        _ => return Err(ErrorCode::InvalidMessage.into()),
    };
    let _permit = state
        .psf_keys
        .preparation_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ErrorCode::LimitExceeded)?;
    let limit = (state.config.read().await.model_library.max_upload_bytes as usize).min(MAX_SOURCE);
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(30), to_bytes(body, limit))
        .await
        .map_err(|_| ErrorCode::Offline)?
        .map_err(|_| LibraryError::too_large())?;
    let session = session_binding(&headers)?;
    let keys = state.psf_keys.clone();
    let work_session = session.clone();
    // Reuse the library's bounded CPU workers. Secret diagnostics never include source bytes.
    let result = blocking(move || {
        Ok(keys.authorize(
            &bytes,
            actor.uuid,
            &work_session,
            purpose,
            crate::storage::now_seconds(),
        ))
    })
    .await?
    .map_err(failure)?;
    let current = actor_identity(&state, &headers).await?;
    if current.uuid != actor.uuid || session_binding(&headers)? != session {
        return Err(ErrorCode::Unauthenticated.into());
    }
    state
        .psf_keys
        .verify(
            &result.grant,
            actor.uuid,
            &session,
            &result.content_sha256,
            purpose,
            crate::storage::now_seconds(),
        )
        .map_err(failure)?;
    let mut response = Json(result).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("pragma", "no-cache".parse().unwrap());
    Ok(response)
}

/// Call only after the existing resource consumer has admitted this exact stored path.
pub(crate) async fn consume_stored(
    state: &AppState,
    headers: &HeaderMap,
    account: Uuid,
    path: std::path::PathBuf,
) -> Result<crate::service::psf::KeyGrant> {
    let _permit = state
        .psf_keys
        .preparation_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ErrorCode::LimitExceeded)?;
    let session = session_binding(headers)?;
    let work_session = session.clone();
    let keys = state.psf_keys.clone();
    let result = blocking(move || {
        let inspected = crate::service::package::inspect(&path)?;
        if inspected.model_type != crate::protocol::v1::ModelType::ModelPsf {
            return Err(ErrorCode::UnsupportedCapability);
        }
        let source = inspected.psf_source.ok_or(ErrorCode::InvalidMessage)?;
        Ok(keys.consume_authorized(
            &source,
            account,
            &work_session,
            crate::storage::now_seconds(),
        ))
    })
    .await?
    .map_err(failure)?;
    if super::player_identity(state, headers)? != account || session_binding(headers)? != session {
        return Err(ErrorCode::Unauthenticated.into());
    }
    state
        .psf_keys
        .verify(
            &result.grant,
            account,
            &session,
            &result.content_sha256,
            Purpose::Consumption,
            crate::storage::now_seconds(),
        )
        .map_err(failure)?;
    Ok(result)
}

pub(crate) fn consumption_response(grant: crate::service::psf::KeyGrant) -> Response {
    let mut response = Json(grant).into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("pragma", "no-cache".parse().unwrap());
    response
}
