use super::*;
use crate::service::library_admin::{AdminConfig, AdminCredentials};
use axum::extract::{State, rejection::JsonRejection};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    operation_id: String,
    player_name: String,
    password: String,
    remember: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    operation_id: String,
    refresh_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logout {
    operation_id: String,
}

#[derive(Clone)]
struct AdminScope {
    player: Uuid,
    origin: String,
    instance: String,
    config: AdminConfig,
}

impl AdminScope {
    fn binding(&self) -> AdminBinding<'_> {
        AdminBinding {
            player: self.player,
            origin: &self.origin,
            instance: &self.instance,
            config: &self.config,
        }
    }
}

async fn configured(state: &AppState, headers: &HeaderMap, uuid: Uuid) -> Result<AdminScope> {
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
    Ok(AdminScope {
        player: uuid,
        origin: origin(headers)?,
        instance: format!(
            "{}:{}",
            state.protocol_sessions.instance_id, state.protocol_sessions.server_epoch
        ),
        config,
    })
}

async fn current_config_guard(
    state: &AppState,
    scope: &AdminScope,
) -> Result<tokio::sync::OwnedRwLockReadGuard<crate::state::Config>> {
    let config = state.config.clone().read_owned().await;
    if !config.model_library.admins.iter().any(|admin| {
        admin.uuid == scope.player && admin.fingerprint() == scope.config.fingerprint()
    }) {
        return Err(ErrorCode::Unauthenticated.into());
    }
    Ok(config)
}

fn credentials_response(credentials: AdminCredentials) -> Result<Response> {
    let mut response = json_response(json!({"access_token": credentials.access_token,
        "refresh_token": credentials.refresh_token, "expires_at": credentials.expires_at}))?;
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("literal header"));
    Ok(response)
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<Login>, JsonRejection>,
) -> Result<Response> {
    use crate::service::library_admin::operations::{
        CredentialOperation, admit_login, secret_fingerprint,
    };
    let uuid = player(&state, &headers)?;
    let input = parse_json(body)?;
    let scope = configured(&state, &headers, uuid).await?;
    let request = serde_json::to_vec(&(
        input.player_name.clone(),
        input.remember,
        secret_fingerprint(&input.password),
    ))
    .map_err(|_| ErrorCode::Internal)?;
    let operation =
        CredentialOperation::new(&scope.binding(), "login", &input.operation_id, &request)?;
    let guard = current_config_guard(&state, &scope).await?;
    let storage = state.storage.clone();
    let replay_scope = scope.clone();
    let replay_operation = operation.clone();
    let replay = blocking(move || {
        storage.library_admin_replay(
            &replay_scope.binding(),
            &replay_operation,
            crate::storage::now_seconds(),
        )
    })
    .await?;
    drop(guard);
    if let Some(credentials) = replay {
        return credentials_response(credentials);
    }
    admit_login(&scope.binding())?;
    scope
        .config
        .verify_password(uuid, &input.player_name, input.password)
        .await?;
    // The guard covers only local SQLite work, never password work or network I/O.
    let guard = current_config_guard(&state, &scope).await?;
    let storage = state.storage.clone();
    let credentials = blocking(move || {
        storage.library_admin_issue_operation(
            &scope.binding(),
            &operation,
            input.remember,
            crate::storage::now_seconds(),
        )
    })
    .await?;
    drop(guard);
    credentials_response(credentials)
}

pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<Refresh>, JsonRejection>,
) -> Result<Response> {
    use crate::service::library_admin::operations::{CredentialOperation, secret_fingerprint};
    let uuid = player(&state, &headers)?;
    let input = parse_json(body)?;
    let scope = configured(&state, &headers, uuid).await?;
    let operation = CredentialOperation::new(
        &scope.binding(),
        "refresh",
        &input.operation_id,
        &secret_fingerprint(&input.refresh_token),
    )?;
    let guard = current_config_guard(&state, &scope).await?;
    let storage = state.storage.clone();
    let credentials = blocking(move || {
        storage.library_admin_refresh_operation(
            &scope.binding(),
            &operation,
            &input.refresh_token,
            crate::storage::now_seconds(),
        )
    })
    .await?;
    drop(guard);
    credentials_response(credentials)
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<Logout>, JsonRejection>,
) -> Result<Response> {
    use crate::service::library_admin::operations::{CredentialOperation, secret_fingerprint};
    let uuid = player(&state, &headers)?;
    let input = parse_json(body)?;
    let scope = configured(&state, &headers, uuid).await?;
    let token = headers
        .get("x-omnimorph-admin")
        .and_then(|value| value.to_str().ok())
        .ok_or(ErrorCode::Unauthenticated)?
        .to_owned();
    let operation = CredentialOperation::new(
        &scope.binding(),
        "logout",
        &input.operation_id,
        &secret_fingerprint(&token),
    )?;
    let guard = current_config_guard(&state, &scope).await?;
    let storage = state.storage.clone();
    blocking(move || {
        storage.library_admin_logout_operation(
            &scope.binding(),
            &operation,
            &token,
            crate::storage::now_seconds(),
        )
    })
    .await?;
    drop(guard);
    json_response(json!({"logged_out": true}))
}
