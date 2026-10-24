//! Owner-only fixed source bodies, including sources absent from the currently worn outfit.
use super::*;
use crate::{
    protocol::v1::AvatarDescriptor,
    service::{
        outfit::{ResourceKind, ResourceRef},
        resource::DownloadPermit,
    },
};
use axum::{
    body::{Body, Bytes},
    extract::{State, rejection::JsonRejection},
};
use tokio::io::AsyncReadExt;

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BodyInput {
    pub reference: ResourceRef,
    pub resource_kind: ResourceKind,
    pub raw_sha256: String,
}

async fn authorize(state: &AppState, owner: Uuid, input: BodyInput) -> Result<AvatarDescriptor> {
    crate::storage::library::validate_hash("RAW_SHA256", &input.raw_sha256)?;
    let storage = state.storage.clone();
    let limits = state.config.read().await.outfit.limits.clone();
    blocking(move || {
        let binding = storage
            .outfit_resolve_local(owner, &input.reference, input.resource_kind, &limits)?
            .ok_or(ErrorCode::NotFound)?;
        if binding.raw_sha256 != input.raw_sha256 {
            return Err(ErrorCode::Conflict);
        }
        let descriptor = storage.avatar(owner, &binding.asset_id, binding.asset_revision)?;
        storage.library_ensure_verified_blob(&descriptor)?;
        if storage
            .outfit_resolve_local(owner, &input.reference, input.resource_kind, &limits)?
            .as_ref()
            != Some(&binding)
        {
            return Err(ErrorCode::Conflict);
        }
        Ok(descriptor)
    })
    .await
}

struct SourceStream {
    state: AppState,
    headers: HeaderMap,
    owner: Uuid,
    input: BodyInput,
    descriptor: AvatarDescriptor,
    file: tokio::fs::File,
    remaining: u64,
    _permit: DownloadPermit,
}

pub async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<BodyInput>, JsonRejection>,
) -> Result<Response> {
    let owner = player(&state, &headers)?;
    let input = parse_json(body)?;
    let permit = DownloadPermit::acquire(owner)?;
    let descriptor = authorize(&state, owner, input.clone()).await?;
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
        SourceStream {
            state,
            headers,
            owner,
            input,
            descriptor,
            file,
            remaining: length,
            _permit: permit,
        },
        |mut stream| async move {
            if stream.remaining == 0 {
                return Ok(None);
            }
            let denied = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "private source unavailable",
                )
            };
            if player_identity(&stream.state, &stream.headers).map_err(|_| denied())?
                != stream.owner
            {
                return Err(denied());
            }
            let current = authorize(&stream.state, stream.owner, stream.input.clone())
                .await
                .map_err(|_| denied())?;
            if current != stream.descriptor {
                return Err(denied());
            }
            let mut bytes = vec![0; stream.remaining.min(32 * 1024) as usize];
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                stream.file.read_exact(&mut bytes),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "private source read timed out",
                )
            })??;
            stream.remaining -= bytes.len() as u64;
            Ok::<_, std::io::Error>(Some((Bytes::from(bytes), stream)))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    for (key, value) in [
        ("content-type", "application/octet-stream".to_owned()),
        ("content-length", length.to_string()),
        ("x-omnimorph-sha256", hash),
        ("cache-control", "private, no-store".to_owned()),
    ] {
        response
            .headers_mut()
            .insert(key, value.parse().map_err(|_| ErrorCode::Internal)?);
    }
    Ok(response)
}
