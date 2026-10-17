use super::*;
use crate::service::library::transfers::{CompleteInput, TransferInput};
use crate::service::resource::DownloadPermit;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State, rejection::JsonRejection},
};
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub async fn create(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<TransferInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_transfer(actor, &id, &input)).await?)
}

pub async fn complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<CompleteInput>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(
        blocking(move || state.storage.library_transfer_complete(actor, &id, &input)).await?,
    )
}

struct TransferStream {
    state: AppState,
    headers: HeaderMap,
    id: String,
    file: tokio::fs::File,
    remaining: u64,
    _permit: DownloadPermit,
}

pub async fn download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let permit = DownloadPermit::acquire(actor.uuid)?;
    let storage = state.storage.clone();
    let transfer = id.clone();
    let resource = blocking(move || storage.library_transfer_file(actor, &transfer)).await?;
    let file = tokio::fs::File::open(resource.path)
        .await
        .map_err(|_| ErrorCode::NotFound)?;
    if file
        .metadata()
        .await
        .map_err(|_| ErrorCode::Internal)?
        .len()
        != resource.length
    {
        return Err(ErrorCode::Internal.into());
    }
    let stream = futures_util::stream::try_unfold(
        TransferStream {
            state,
            headers,
            id,
            file,
            remaining: resource.length,
            _permit: permit,
        },
        |mut stream| async move {
            if stream.remaining == 0 {
                return Ok(None);
            }
            let failed = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "library transfer unavailable",
                )
            };
            let actor = actor_identity(&stream.state, &stream.headers)
                .await
                .map_err(|_| failed())?;
            let storage = stream.state.storage.clone();
            let id = stream.id.clone();
            blocking(move || storage.library_transfer_file(actor, &id).map(|_| ()))
                .await
                .map_err(|_| failed())?;
            let mut bytes = vec![0; stream.remaining.min(32 * 1024) as usize];
            tokio::time::timeout(Duration::from_secs(30), stream.file.read_exact(&mut bytes))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "library transfer timed out")
                })??;
            stream.remaining -= bytes.len() as u64;
            if stream.remaining == 0 {
                let storage = stream.state.storage.clone();
                let id = stream.id.clone();
                blocking(move || storage.library_transfer_sent(actor, &id))
                    .await
                    .map_err(|_| failed())?;
            }
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
        resource
            .length
            .to_string()
            .parse()
            .map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "x-omnimorph-sha256",
        resource
            .raw_sha256
            .parse()
            .map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "cache-control",
        "private, no-store".parse().expect("literal header"),
    );
    Ok(response)
}
