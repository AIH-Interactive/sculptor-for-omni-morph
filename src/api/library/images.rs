use super::*;
use crate::service::library::Mutation;
use axum::{
    body::{Body, Bytes},
    extract::{
        Path, State,
        rejection::{BytesRejection, JsonRejection},
    },
};
use tokio_util::io::ReaderStream;

pub async fn put_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Bytes, BytesRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let bytes = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            LibraryError::too_large()
        } else {
            ErrorCode::InvalidMessage.into()
        }
    })?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(LibraryError::too_large());
    }
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !matches!(content_type, "image/png" | "image/jpeg") {
        return Err(LibraryError::unsupported());
    }
    let operation = headers
        .get("x-omnimorph-operation")
        .and_then(|value| value.to_str().ok())
        .ok_or(ErrorCode::InvalidMessage)?
        .to_owned();
    let expected_revision = headers
        .get("x-omnimorph-expected-revision")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or(ErrorCode::InvalidMessage)?;
    let input = Mutation {
        operation_id: operation,
        expected_revision,
        display_name: None,
        description: None,
        category_id: None,
        tags: None,
        visibility: None,
        requires_authorization: None,
        reason: None,
    };
    let content_type = content_type.to_owned();
    let result = blocking(move || {
        if crate::service::library::images::inspect(&bytes)?.0 != content_type {
            return Err(ErrorCode::InvalidMessage);
        }
        state.storage.library_image_put(actor, &id, &input, &bytes)
    })
    .await?;
    json_response(result)
}

pub async fn delete_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    let input = parse_json(body)?;
    json_response(blocking(move || state.storage.library_image_delete(actor, &id, &input)).await?)
}

pub async fn image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let actor = actor(&state, &headers).await?;
    // Check visibility separately to distinguish a missing image from a hidden model.
    let storage = state.storage.clone();
    let checked_id = id.clone();
    blocking(move || {
        storage.library_read(|database| {
            crate::service::library::model(database, &checked_id, actor, true).map(|_| ())
        })
    })
    .await?;
    let image = blocking(move || state.storage.library_image(actor, &id))
        .await
        .map_err(|error| {
            if error.status == StatusCode::NOT_FOUND {
                LibraryError::no_image()
            } else {
                error
            }
        })?;
    let file = tokio::fs::File::open(image.path)
        .await
        .map_err(|_| LibraryError::no_image())?;
    if file
        .metadata()
        .await
        .map_err(|_| ErrorCode::Internal)?
        .len()
        != image.length
    {
        return Err(ErrorCode::Internal.into());
    }
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    response.headers_mut().insert(
        "content-type",
        image.media_type.parse().map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "content-length",
        image
            .length
            .to_string()
            .parse()
            .map_err(|_| ErrorCode::Internal)?,
    );
    response.headers_mut().insert(
        "cache-control",
        "private, no-store".parse().expect("literal header"),
    );
    Ok(response)
}
