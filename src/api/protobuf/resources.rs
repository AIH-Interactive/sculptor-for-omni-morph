use super::*;
use crate::service::resource;
use axum::{
    body::Body,
    extract::{Path, State},
};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

pub async fn upload(
    State(state): State<AppState>,
    Path(operation): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let scope = match file_scope(&state, &headers) {
        Ok(scope) => scope,
        Err(code) => return failure(code, 0, None),
    };
    if headers
        .get("content-type")
        .and_then(|header| header.to_str().ok())
        != Some("application/octet-stream")
    {
        return failure(ErrorCode::InvalidMessage, 0, Some(&scope));
    }
    let mut upload = match resource::begin_upload(&state, scope.owner, operation) {
        Ok(upload) => upload,
        Err(code) => return failure(code, 0, Some(&scope)),
    };
    let limit = upload.descriptor.raw_length;
    let expected = upload.descriptor.raw_sha256.clone();
    let path = upload.path.clone();
    let received = tokio::time::timeout(Duration::from_secs(60), async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|_| ErrorCode::Internal)?;
        let mut stream = body.into_data_stream();
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut count = 0u64;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|_| ErrorCode::InvalidMessage)?;
            count = count
                .checked_add(bytes.len() as u64)
                .ok_or(ErrorCode::LimitExceeded)?;
            if count > limit {
                return Err(ErrorCode::LimitExceeded);
            }
            state
                .protocol_sessions
                .authorize(&state, &scope.id, scope.epoch)?;
            hash.update(&bytes);
            file.write_all(&bytes)
                .await
                .map_err(|_| ErrorCode::Internal)?;
        }
        if count != limit || hash.finish().as_ref() != expected {
            return Err(ErrorCode::InvalidMessage);
        }
        file.sync_all().await.map_err(|_| ErrorCode::Internal)?;
        Ok::<_, ErrorCode>(())
    })
    .await;
    match received {
        Ok(Ok(())) => {
            upload.seal();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(code)) => failure(code, 0, Some(&scope)),
        Err(_) => failure(ErrorCode::Offline, 0, Some(&scope)),
    }
}

pub async fn download(
    State(state): State<AppState>,
    Path((owner, asset, revision)): Path<(Uuid, String, u64)>,
    headers: HeaderMap,
) -> Response {
    let scope = match file_scope(&state, &headers) {
        Ok(scope) => scope,
        Err(code) => return failure(code, 0, None),
    };
    let result = async {
        if revision == 0 {
            return Err(ErrorCode::InvalidMessage);
        }
        let avatar = state.storage.avatar(owner, &asset, revision)?;
        let path = state.storage.blob_path(&avatar.raw_sha256)?;
        let file = fs::File::open(path)
            .await
            .map_err(|_| ErrorCode::NotFound)?;
        if file
            .metadata()
            .await
            .map_err(|_| ErrorCode::Internal)?
            .len()
            != avatar.raw_length
        {
            return Err(ErrorCode::Internal);
        }
        let mut response = Response::new(Body::from_stream(ReaderStream::new(
            file.take(avatar.raw_length),
        )));
        response
            .headers_mut()
            .insert("content-type", "application/octet-stream".parse().unwrap());
        response.headers_mut().insert(
            "content-length",
            avatar
                .raw_length
                .to_string()
                .parse()
                .map_err(|_| ErrorCode::Internal)?,
        );
        Ok::<_, ErrorCode>(response)
    }
    .await;
    result.unwrap_or_else(|code| failure(code, 0, Some(&scope)))
}
