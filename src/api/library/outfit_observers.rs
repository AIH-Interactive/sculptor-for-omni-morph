//! Source bodies are scoped to current authorized equipment, not catalog acquisition.
use super::*;
use crate::{protocol::v1::AvatarDescriptor, service::resource::DownloadPermit};
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
};
use serde::Deserialize;
use std::time::Duration;
use tokio::io::AsyncReadExt;

// Configuration plus at most 32 fixed declarations accepted by the existing
// 64KiB metadata request boundary, with bounded resource/envelope fields.
const SNAPSHOT_JSON_MAX_BYTES: usize = crate::service::outfit::MAX_JSON_BYTES
    + 8192
    + crate::service::outfit::MAX_INSTANCES * (crate::service::outfit::MAX_JSON_BYTES + 4096);

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceQuery {
    pub revision: u64,
}

pub async fn snapshot(
    State(state): State<AppState>,
    Path(owner): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response> {
    let observer = player(&state, &headers)?;
    let origin = origin(&headers)?;
    let limits = state.config.read().await.outfit.limits.clone();
    let storage = state.storage.clone();
    let snapshot =
        blocking(move || storage.outfit_project(owner, observer, &origin, &limits)).await?;
    json_response_limited(
        serde_json::to_value(snapshot).map_err(|_| ErrorCode::Internal)?,
        SNAPSHOT_JSON_MAX_BYTES,
    )
}

struct ResourceStream {
    state: AppState,
    headers: HeaderMap,
    owner: Uuid,
    observer: Uuid,
    origin: String,
    id: String,
    revision: u64,
    descriptor: AvatarDescriptor,
    file: tokio::fs::File,
    remaining: u64,
    _permit: DownloadPermit,
}

async fn authorize(
    state: &AppState,
    owner: Uuid,
    observer: Uuid,
    origin: String,
    revision: u64,
    id: String,
) -> Result<AvatarDescriptor> {
    let storage = state.storage.clone();
    let limits = state.config.read().await.outfit.limits.clone();
    blocking(move || storage.outfit_resource(owner, observer, &origin, revision, &id, &limits))
        .await
}

pub async fn download(
    State(state): State<AppState>,
    Path((owner, id)): Path<(Uuid, String)>,
    Query(query): Query<ResourceQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    let observer = player(&state, &headers)?;
    let origin = origin(&headers)?;
    let permit = DownloadPermit::acquire(observer)?;
    let descriptor = authorize(
        &state,
        owner,
        observer,
        origin.clone(),
        query.revision,
        id.clone(),
    )
    .await?;
    let file = tokio::fs::File::open(state.storage.blob_path(&descriptor.raw_sha256)?)
        .await
        .map_err(|_| ErrorCode::NotFound)?;
    if file
        .metadata()
        .await
        .map_err(|_| ErrorCode::Internal)?
        .len()
        != descriptor.raw_length
    {
        return Err(ErrorCode::Internal.into());
    }
    let length = descriptor.raw_length;
    let hash = faster_hex::hex_string(&descriptor.raw_sha256);
    let stream = futures_util::stream::try_unfold(
        ResourceStream {
            state,
            headers,
            owner,
            observer,
            origin,
            id,
            revision: query.revision,
            remaining: length,
            descriptor,
            file,
            _permit: permit,
        },
        |mut stream| async move {
            if stream.remaining == 0 {
                return Ok(None);
            }
            let denied = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "outfit resource unavailable",
                )
            };
            if player_identity(&stream.state, &stream.headers).map_err(|_| denied())?
                != stream.observer
            {
                return Err(denied());
            }
            let current = authorize(
                &stream.state,
                stream.owner,
                stream.observer,
                stream.origin.clone(),
                stream.revision,
                stream.id.clone(),
            )
            .await
            .map_err(|_| denied())?;
            if current != stream.descriptor {
                return Err(denied());
            }
            let mut bytes = vec![0; stream.remaining.min(32 * 1024) as usize];
            tokio::time::timeout(Duration::from_secs(30), stream.file.read_exact(&mut bytes))
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "outfit resource read timed out",
                    )
                })??;
            stream.remaining -= bytes.len() as u64;
            Ok::<_, std::io::Error>(Some((Bytes::from(bytes), stream)))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        "content-type",
        "application/octet-stream".parse().expect("literal header"),
    );
    response.headers_mut().insert(
        "content-length",
        length
            .to_string()
            .parse()
            .map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "x-omnimorph-sha256",
        hash.parse().map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "cache-control",
        "private, no-store".parse().expect("literal header"),
    );
    Ok(response)
}
