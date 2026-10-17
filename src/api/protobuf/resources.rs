use super::*;
use crate::service::resource;
use axum::{
    body::Body,
    extract::{Path, State},
};
use tokio::{fs, io::AsyncReadExt};
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
    let received = resource::receive_upload(&mut upload, body, || {
        state
            .protocol_sessions
            .authorize(&state, &scope.id, scope.epoch)
            .map(|_| ())
    })
    .await;
    match received {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(code) => failure(code, 0, Some(&scope)),
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
        let storage = state.storage.clone();
        let asset_id = asset.clone();
        let observer = scope.owner;
        crate::service::library::work(move || {
            storage.library_resource_access(
                owner,
                &asset_id,
                revision,
                crate::service::library_policy::Actor {
                    uuid: observer,
                    admin_context: false,
                    granted: false,
                },
            )
        })
        .await?;
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
