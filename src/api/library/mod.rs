//! Independent HTTP JSON extension; frozen Protobuf v1 remains unchanged.
mod admin;
mod admin_preview;
mod reports;
mod images;
mod models;
mod outfits;
mod private_uploads;
mod outfit_observers;
mod outfit_events;
mod outfit_state;
mod outfit_local_sources;
mod collection;
mod transfers;
mod uploads;
mod wardrobe;
mod appeals;

#[cfg(test)]
mod tests;

use crate::{
    AppState,
    protocol::v1::ErrorCode,
    service::{library_admin::AdminBinding, library_policy::Actor},
};
use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::{Value, json};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, LibraryError>;

pub struct LibraryError {
    status: StatusCode,
    code: &'static str,
}

impl LibraryError {
    pub fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "TOO_LARGE",
        }
    }
    pub fn no_image() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NO_IMAGE",
        }
    }
    pub fn unsupported() -> Self {
        Self {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            code: "UNSUPPORTED_FORMAT",
        }
    }
}

impl From<ErrorCode> for LibraryError {
    fn from(code: ErrorCode) -> Self {
        let (status, code) = match code {
            ErrorCode::Unauthenticated => (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            ErrorCode::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            ErrorCode::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            ErrorCode::Conflict => (StatusCode::CONFLICT, "CONFLICT"),
            ErrorCode::LimitExceeded => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            ErrorCode::Offline => (StatusCode::SERVICE_UNAVAILABLE, "BUSY"),
            ErrorCode::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"),
            ErrorCode::UnsupportedCapability | ErrorCode::UnsupportedVersion => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, "UNSUPPORTED_FORMAT")
            }
            _ => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
        };
        Self { status, code }
    }
}

impl IntoResponse for LibraryError {
    fn into_response(self) -> Response {
        let request_id = crate::service::library::random_id();
        let mut response = (self.status, Json(json!({"code": self.code,
            "localization_key": format!("library.error.{}", self.code.to_ascii_lowercase()),
            "request_id": request_id,
            "retryable": matches!(self.status, StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE),
        }))).into_response();
        if let Ok(value) = request_id.parse() {
            response.headers_mut().insert("x-request-id", value);
        }
        response
    }
}

pub fn json_response(value: Value) -> Result<Response> {
    json_response_limited(value, 65536)
}

fn json_response_limited(value: Value, max_bytes: usize) -> Result<Response> {
    let bytes = serde_json::to_vec(&value).map_err(|_| ErrorCode::Internal)?;
    if bytes.len() > max_bytes {
        return Err(ErrorCode::Internal.into());
    }
    Ok(([("content-type", "application/json")], bytes).into_response())
}

pub async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::result::Result<T, ErrorCode> + Send + 'static,
{
    crate::service::library::work(work)
        .await
        .map_err(Into::into)
}

pub fn parse_json<T: serde::de::DeserializeOwned>(
    input: std::result::Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T> {
    input.map(|Json(value)| value).map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            LibraryError::too_large()
        } else {
            ErrorCode::InvalidMessage.into()
        }
    })
}

pub(crate) fn origin(headers: &HeaderMap) -> Result<String> {
    if headers.get_all("x-omnimorph-origin").iter().count() != 1
        || headers.get_all("host").iter().count() != 1
    {
        return Err(ErrorCode::Forbidden.into());
    }
    let origin = headers
        .get("x-omnimorph-origin")
        .and_then(|value| value.to_str().ok())
        .ok_or(ErrorCode::Forbidden)?;
    let url = reqwest::Url::parse(origin).map_err(|_| ErrorCode::InvalidMessage)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ErrorCode::InvalidMessage.into());
    }
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .ok_or(ErrorCode::Forbidden)?;
    let expected = reqwest::Url::parse(&format!("{}://{host}", url.scheme()))
        .map_err(|_| ErrorCode::Forbidden)?;
    if expected.origin() != url.origin() {
        return Err(ErrorCode::Forbidden.into());
    }
    Ok(url.origin().ascii_serialization())
}

/// Legacy catalog clients lack the explicit namespace header. The proxy scheme
/// labels this server's resources; it never grants access or selects another host.
fn equipment_origin(headers: &HeaderMap) -> Result<String> {
    if headers.contains_key("x-omnimorph-origin") {
        return origin(headers);
    }
    if headers.get_all("host").iter().count() != 1
        || headers.get_all("x-forwarded-proto").iter().count() > 1
    {
        return Err(ErrorCode::Forbidden.into());
    }
    let scheme = match headers.get("x-forwarded-proto") {
        Some(value) => value.to_str().map_err(|_| ErrorCode::InvalidMessage)?,
        None => "http", // The native listener uses HTTP; TLS is terminated by the proxy.
    };
    if !matches!(scheme, "http" | "https") {
        return Err(ErrorCode::InvalidMessage.into());
    }
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .ok_or(ErrorCode::Forbidden)?;
    let mut scoped = headers.clone();
    scoped.insert(
        "x-omnimorph-origin",
        format!("{scheme}://{host}")
            .parse()
            .map_err(|_| ErrorCode::InvalidMessage)?,
    );
    origin(&scoped)
}

pub(crate) fn player(state: &AppState, headers: &HeaderMap) -> Result<Uuid> {
    player_identity(state, headers)
}

pub(crate) fn player_identity(state: &AppState, headers: &HeaderMap) -> Result<Uuid> {
    for name in [
        "authorization",
        "x-omnimorph-auth-mode",
        "x-omnimorph-epoch",
        "x-omnimorph-admin",
    ] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(ErrorCode::Unauthenticated.into());
        }
    }
    if headers.contains_key("token") {
        return Err(ErrorCode::Unauthenticated.into());
    }
    let mode = headers
        .get("x-omnimorph-auth-mode")
        .map(|value| value.to_str().unwrap_or("invalid"))
        .unwrap_or("protobuf");
    let uuid = match mode {
        "protobuf" => crate::api::protobuf::file_scope(state, headers)?.owner,
        "figura" => {
            if headers.contains_key("x-omnimorph-epoch") {
                return Err(ErrorCode::Unauthenticated.into());
            }
            let token = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or(ErrorCode::Unauthenticated)?;
            crate::service::session::authenticate(state, token)
                .map_err(|_| ErrorCode::Unauthenticated)?
                .uuid
        }
        _ => return Err(ErrorCode::Unauthenticated.into()),
    };
    Ok(uuid)
}

pub(crate) async fn actor(state: &AppState, headers: &HeaderMap) -> Result<Actor> {
    actor_identity(state, headers).await
}

pub(crate) async fn actor_identity(state: &AppState, headers: &HeaderMap) -> Result<Actor> {
    let uuid = player_identity(state, headers)?;
    let mut actor = Actor {
        uuid,
        admin_context: false,
        granted: false,
    };
    if let Some(token) = headers.get("x-omnimorph-admin") {
        let token = token
            .to_str()
            .map_err(|_| ErrorCode::Unauthenticated)?
            .to_owned();
        let origin = origin(headers)?;
        let config = state
            .config
            .read()
            .await
            .model_library
            .admins
            .iter()
            .find(|admin| admin.uuid == uuid)
            .cloned()
            .ok_or(ErrorCode::Forbidden)?;
        let storage = state.storage.clone();
        let instance = format!(
            "{}:{}",
            state.protocol_sessions.instance_id, state.protocol_sessions.server_epoch
        );
        blocking(move || {
            storage.library_admin_authorize(
                &AdminBinding {
                    player: uuid,
                    origin: &origin,
                    instance: &instance,
                    config: &config,
                },
                &token,
                crate::storage::now_seconds(),
            )
        })
        .await?;
        actor.admin_context = true;
    }
    Ok(actor)
}

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/library/v1/outfits/current",
            get(outfits::current)
                .put(outfits::apply)
                .layer(DefaultBodyLimit::max(outfits::APPLY_JSON_MAX_BYTES)),
        )
        .route("/api/library/v1/outfit-events", get(outfit_events::upgrade))
        .route("/api/library/v1/outfits/players/:owner/instances/:instance/state", get(outfit_state::read))
        .route("/api/library/v1/outfits/instances/:instance/state", post(outfit_state::write))
        .route("/api/library/v1/outfits/players/:owner", get(outfit_observers::snapshot))
        .route("/api/library/v1/outfits/players/:owner/resources/:id", get(outfit_observers::download))
        .route("/api/library/v1/outfits/resources/local", put(outfits::bind_local))
        .route("/api/library/v1/outfits/resources/local/resolve", post(outfits::resolve_local))
        .route("/api/library/v1/outfits/resources/local/body", post(outfit_local_sources::download))
        .route("/api/library/v1/outfits/resources/uploads", post(private_uploads::reserve))
        .route(
            "/api/library/v1/outfits/resources/uploads/:operation/body",
            put(private_uploads::upload).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/library/v1/outfits/resources/uploads/:operation/commit",
            post(private_uploads::commit),
        )
        .route(
            "/api/library/v1/outfit-presets",
            get(outfits::list).post(outfits::save),
        )
        .route("/api/library/v1/outfit-presets/:id", get(outfits::read))
        .route("/api/library/v1/collection", get(collection::list))
        .route(
            "/api/library/v1/collection/:id",
            put(collection::join).delete(collection::remove),
        )
        .route("/api/library/v1/collection/:id/outfit", put(collection::outfit))
        .route("/api/library/v1/models/:id/equip", post(models::equip))
        .route(
            "/api/library/v1/models/:id/transfers",
            post(transfers::create),
        )
        .route("/api/library/v1/transfers/:id", get(transfers::download))
        .route(
            "/api/library/v1/transfers/:id/complete",
            post(transfers::complete),
        )
        .route("/api/library/v1/uploads", post(uploads::reserve))
        .route("/api/library/v1/uploads/outfit", post(outfits::publish))
        .route("/api/library/v1/models/:id/outfit", get(outfits::published))
        .route(
            "/api/library/v1/uploads/:operation",
            axum::routing::delete(uploads::cancel),
        )
        .route(
            "/api/library/v1/uploads/:operation/commit",
            post(uploads::commit),
        )
        .route(
            "/api/library/v1/uploads/:operation/body",
            put(uploads::upload).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/library/v1/models/:id/image",
            get(images::image)
                .put(images::put_image)
                .delete(images::delete_image)
                .layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/library/v1/capabilities", get(models::capabilities))
        .route("/api/library/v1/session", get(models::session))
        .route("/api/library/v1/models", get(models::list))
        .route(
            "/api/library/v1/models/:id",
            get(models::detail)
                .patch(models::patch)
                .delete(models::delete),
        )
        .route(
            "/api/library/v1/models/:id/favorite",
            put(models::favorite).delete(models::unfavorite),
        )
        .route("/api/library/v1/models/:id/grants", get(models::grants))
        .route(
            "/api/library/v1/models/:id/grants/:player",
            put(models::grant).delete(models::revoke),
        )
        .route("/api/library/v1/players/resolve", get(models::resolve))
        .route("/api/library/v1/admin/login", post(admin::login))
        .route("/api/library/v1/admin/refresh", post(admin::refresh))
        .route("/api/library/v1/admin/logout", post(admin::logout))
        .route("/api/library/v1/models/:id/reports", post(reports::submit))
        .route("/api/library/v1/admin/reports", get(reports::list))
        .route("/api/library/v1/admin/reports/:id/resolve", post(reports::resolve))
        .route("/api/library/v1/admin/wardrobe", get(wardrobe::list))
        .route(
            "/api/library/v1/admin/wardrobe-preview/:kind/:id",
            get(admin_preview::download),
        )
        .route("/api/library/v1/admin/wardrobe/:owner/reset", post(wardrobe::reset))
        .route("/api/library/v1/admin/wardrobe/:owner/block", post(wardrobe::block))
        .route("/api/library/v1/appeal-targets", get(appeals::targets))
        .route("/api/library/v1/wardrobe", get(wardrobe::own).patch(wardrobe::visibility))
        .route("/api/library/v1/appeals", get(appeals::mine).post(appeals::submit))
        .route("/api/library/v1/admin/appeals", get(appeals::list))
        .route("/api/library/v1/admin/appeals/:id/resolve", post(appeals::resolve))
        .route("/api/library/v1/admin/appeals/:id/unblock", post(appeals::unblock))
        .route(
            "/api/library/v1/admin/models/:id/block",
            put(models::block).delete(models::unblock),
        )
        .route("/api/library/v1/admin/models/:id/restore", post(models::restore))
        .route("/api/library/v1/admin/recycle", get(wardrobe::recycle_list))
        .route("/api/library/v1/admin/wardrobe-recycle/:owner/:archive/restore", post(wardrobe::restore))
        .route("/api/library/v1/admin/wardrobe-recycle/:owner/:archive/purge", post(wardrobe::purge))
        .route("/api/library/v1/admin/models/:id/purge", post(models::purge))
        .layer(DefaultBodyLimit::max(65536))
        .route_layer(axum::middleware::from_fn_with_state(state, dispatch))
}

async fn dispatch(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::{
        body::{Body, to_bytes},
        http::Method,
    };
    let started = std::time::Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "library".into());
    let result: Result<(Response, usize)> = async {
        let uuid = player_identity(&state, request.headers())?;
        let binary = method == Method::PUT
            && (request.uri().path().ends_with("/body")
                || request.uri().path().ends_with("/image"));
        let (request, request_bytes) = if binary {
            (request, 0)
        } else {
            let max_bytes = if method == Method::PUT && route == "/api/library/v1/outfits/current" {
                outfits::APPLY_JSON_MAX_BYTES
            } else {
                65536
            };
            let (parts, body) = request.into_parts();
            let bytes =
                tokio::time::timeout(std::time::Duration::from_secs(15), to_bytes(body, max_bytes))
                    .await
                    .map_err(|_| ErrorCode::Offline)?
                    .map_err(|_| LibraryError::too_large())?;
            let length = bytes.len();
            (
                axum::http::Request::from_parts(parts, Body::from(bytes)),
                length,
            )
        };
        state
            .protocol_sessions
            .charge(uuid, request_bytes.saturating_add(1024))?;
        let mut response = next.run(request).await;
        if response.status().is_client_error()
            && response
                .headers()
                .get("content-type")
                .and_then(|header| header.to_str().ok())
                != Some("application/json")
        {
            let error = match response.status() {
                StatusCode::PAYLOAD_TOO_LARGE => LibraryError::too_large(),
                StatusCode::NOT_FOUND => ErrorCode::NotFound.into(),
                StatusCode::METHOD_NOT_ALLOWED => LibraryError {
                    status: StatusCode::METHOD_NOT_ALLOWED,
                    code: "METHOD_NOT_ALLOWED",
                },
                StatusCode::UNSUPPORTED_MEDIA_TYPE => LibraryError::unsupported(),
                _ => ErrorCode::InvalidMessage.into(),
            };
            response = error.into_response();
        }
        Ok((response, request_bytes))
    }
    .await;
    let (mut response, request_bytes) = match result {
        Ok(result) => result,
        Err(error) => (error.into_response(), 0),
    };
    if !response.headers().contains_key("x-request-id") {
        if let Ok(value) = crate::service::library::random_id().parse() {
            response.headers_mut().insert("x-request-id", value);
        }
    }
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("literal header"));
    let preview_cache_seconds = state.config.read().await.model_library.preview_cache_seconds;
    response.headers_mut().insert(
        "x-omnimorph-preview-cache-seconds",
        preview_cache_seconds
            .to_string()
            .parse()
            .expect("numeric cache duration"),
    );
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unavailable");
    tracing::info!(request_id, method = %method, route, status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64, request_bytes, "Library request finished");
    response
}
