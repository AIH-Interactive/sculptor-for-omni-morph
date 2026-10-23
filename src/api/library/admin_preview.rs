use super::*;
use crate::service::resource::DownloadPermit;
use crate::storage::library_admin_preview::PreviewQuery;
use axum::{
    body::Body,
    extract::{Path, Query, State},
};
use tokio::io::AsyncReadExt;

struct PreviewStream {
    state: AppState,
    headers: HeaderMap,
    kind: String,
    id: Uuid,
    query: PreviewQuery,
    hash: Vec<u8>,
    file: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    configuration: bool,
    remaining: u64,
    _permit: DownloadPermit,
}

pub async fn download(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, Uuid)>,
    Query(query): Query<PreviewQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    if !actor.admin_context {
        return Err(ErrorCode::Forbidden.into());
    }
    let permit = DownloadPermit::acquire(actor.uuid)?;
    let storage = state.storage.clone();
    let source_kind = kind.clone();
    let source_query = query.clone();
    let configuration =
        blocking(move || storage.recycled_configuration(actor, &source_kind, id)).await?;
    let is_configuration = configuration.is_some();
    let (file, length, digest): (Box<dyn tokio::io::AsyncRead + Unpin + Send>, u64, Vec<u8>) =
        if let Some(bytes) = configuration {
            let length = bytes.len() as u64;
            let digest = crate::storage::sha256(&bytes).to_vec();
            (Box::new(std::io::Cursor::new(bytes)), length, digest)
        } else {
            let storage = state.storage.clone();
            let source_kind = kind.clone();
            let avatar = blocking(move || {
                storage.library_admin_preview(actor, &source_kind, id, &source_query)
            })
            .await?;
            let path = if matches!(kind.as_str(), "RECYCLE" | "LIBRARY_RECYCLE") {
                let storage = state.storage.clone();
                let source_kind = kind.clone();
                let owner = query.owner;
                blocking(move || storage.recycle_preview(actor, &source_kind, id, owner))
                    .await?
                    .1
            } else {
                state.storage.blob_path(&avatar.raw_sha256)?
            };
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|_| ErrorCode::NotFound)?;
            if file
                .metadata()
                .await
                .map_err(|_| ErrorCode::Internal)?
                .len()
                != avatar.raw_length
            {
                return Err(ErrorCode::Internal.into());
            }
            (Box::new(file), avatar.raw_length, avatar.raw_sha256)
        };
    let hash = faster_hex::hex_string(&digest);
    let stream = futures_util::stream::try_unfold(
        PreviewStream {
            state,
            headers,
            kind,
            id,
            query,
            hash: digest,
            file,
            configuration: is_configuration,
            remaining: length,
            _permit: permit,
        },
        |mut stream| async move {
            if stream.remaining == 0 {
                return Ok(None);
            }
            let unavailable = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "administrator preview unavailable",
                )
            };
            let actor = actor_identity(&stream.state, &stream.headers)
                .await
                .map_err(|_| unavailable())?;
            let storage = stream.state.storage.clone();
            let kind = stream.kind.clone();
            let query = stream.query.clone();
            let id = stream.id;
            let configuration = stream.configuration;
            let hash = blocking(move || {
                if configuration {
                    let bytes = storage
                        .recycled_configuration(actor, &kind, id)?
                        .ok_or(ErrorCode::NotFound)?;
                    Ok(crate::storage::sha256(&bytes).to_vec())
                } else {
                    storage
                        .library_admin_preview(actor, &kind, id, &query)
                        .map(|avatar| avatar.raw_sha256)
                }
            })
            .await
            .map_err(|_| unavailable())?;
            if hash != stream.hash {
                return Err(unavailable());
            }
            let mut bytes = vec![0; stream.remaining.min(32 * 1024) as usize];
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                stream.file.read_exact(&mut bytes),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "preview read timed out")
            })??;
            stream.remaining -= bytes.len() as u64;
            Ok::<_, std::io::Error>(Some((bytes, stream)))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    for (name, value) in [
        (
            "content-type",
            if is_configuration {
                "application/json"
            } else {
                "application/octet-stream"
            }
            .to_string(),
        ),
        ("content-length", length.to_string()),
        ("x-omnimorph-sha256", hash),
        ("cache-control", "private, no-store".to_string()),
    ] {
        response
            .headers_mut()
            .insert(name, value.parse().map_err(|_| ErrorCode::Internal)?);
    }
    Ok(response)
}
