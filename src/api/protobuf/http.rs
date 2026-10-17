use super::*;
use crate::service::{equipment, event::CloudEvent, resource};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, State},
};
use uuid::Uuid;

pub async fn capabilities(State(state): State<AppState>) -> Response {
    response(envelope(
        0,
        None,
        envelope::Body::HelloAck(hello_ack(&state).await),
    ))
}

pub async fn rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(error) => {
            return failure(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    ErrorCode::LimitExceeded
                } else {
                    ErrorCode::InvalidMessage
                },
                0,
                None,
            )
        }
    };
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        != Some(CONTENT_TYPE)
    {
        return failure(ErrorCode::InvalidMessage, 0, None);
    }
    let request = match wire::decode(&bytes) {
        Ok(request) => request,
        Err(code) => return failure(code, 0, None),
    };
    let id = request.request_id;
    if id == 0 {
        return failure(ErrorCode::InvalidMessage, id, None);
    }
    if let Some(envelope::Body::Auth(auth)) = &request.body {
        if !request.session_id.is_empty() {
            return failure(ErrorCode::InvalidMessage, id, None);
        }
        return match state
            .protocol_sessions
            .issue(&state, auth, request.connection_epoch)
        {
            Ok(authenticated) => {
                let scope = state.protocol_sessions.authorize(
                    &state,
                    &authenticated.session_id,
                    request.connection_epoch,
                );
                response(envelope(
                    id,
                    scope.as_ref().ok(),
                    envelope::Body::AuthResult(authenticated),
                ))
            }
            Err(code) => failure(code, id, None),
        };
    }
    let scope = match authorize(
        &state,
        &headers,
        &request.session_id,
        request.connection_epoch,
    ) {
        Ok(scope) => scope,
        Err(code) => return failure(code, id, None),
    };
    if let Err(code) = state.protocol_sessions.charge(scope.owner, bytes.len()) {
        return failure(code, id, Some(&scope));
    }
    match dispatch(&state, &scope, request.body.unwrap()).await {
        Ok(body) => response(envelope(id, Some(&scope), body)),
        Err(code) => failure(code, id, Some(&scope)),
    }
}

pub async fn dispatch(
    state: &AppState,
    scope: &ScopedSession,
    body: envelope::Body,
) -> Result<envelope::Body, ErrorCode> {
    use envelope::Body;
    match body {
        Body::ProfileRequest(request) => {
            let owner = Uuid::parse_str(&request.owner).map_err(|_| ErrorCode::InvalidMessage)?;
            resource::ensure_legacy_import(state, owner, false).await?;
            let user = state
                .user_manager
                .get_by_uuid(&owner)
                .map(|user| user.clone())
                .ok_or(ErrorCode::NotFound)?;
            let config = state.config.read().await;
            let badges = config
                .advanced_users
                .get(&owner)
                .map(|settings| Badges {
                    special: settings.special.iter().map(|value| *value as u32).collect(),
                    pride: settings.pride.iter().map(|value| *value as u32).collect(),
                })
                .unwrap_or(Badges {
                    special: vec![0; 6],
                    pride: vec![0; 25],
                });
            let can_upload = state
                .user_manager
                .upload_state(owner, config.limitations.can_upload);
            drop(config);
            Ok(Body::Profile(Profile {
                owner: owner.to_string(),
                rank: user.rank,
                badges: Some(badges),
                equipped: Some(state.storage.equipped(owner)?),
                client_version: user.version,
                last_used: chrono::DateTime::parse_from_rfc3339(&user.last_used)
                    .map(|time| time.timestamp().max(0) as u64)
                    .unwrap_or_default(),
                banned: user.banned,
                can_upload,
            }))
        }
        Body::ServerInfo(_) => {
            let versions = crate::api::figura::info::version(State(state.clone()))
                .await
                .0;
            let motd = crate::utils::get_motd(state.clone()).await;
            Ok(Body::ServerInfo(ServerInfo {
                limits: Some(limits(state).await),
                motd_json: serde_json::to_string(&motd).map_err(|_| ErrorCode::Internal)?,
                release: versions.release,
                prerelease: versions.prerelease,
                allowed_badges: Some(Badges {
                    special: vec![0; 6],
                    pride: vec![0; 25],
                }),
                max_avatars: u32::try_from(state.config.read().await.limitations.max_avatars)
                    .map_err(|_| ErrorCode::LimitExceeded)?,
            }))
        }
        Body::UploadRequest(request) => Ok(Body::TransferGrant(
            resource::grant_upload(state, scope.owner, &request).await?,
        )),
        Body::CommitUpload(request) => Ok(Body::Avatar(
            resource::commit_upload(state, scope.owner, &request).await?,
        )),
        Body::DownloadRequest(request) => {
            let owner = Uuid::parse_str(&request.owner).map_err(|_| ErrorCode::InvalidMessage)?;
            if request.revision == 0 {
                return Err(ErrorCode::InvalidMessage);
            }
            resource::ensure_legacy_import(state, owner, false).await?;
            let avatar = state
                .storage
                .avatar(owner, &request.asset_id, request.revision)?;
            Ok(Body::TransferGrant(TransferGrant {
                operation_id: String::new(),
                relative_path: format!(
                    "/api/protobuf/v1/resources/{owner}/{}/{}",
                    avatar.asset_id, avatar.revision
                ),
                expires_unix_seconds: crate::storage::now_seconds() + 120,
                max_bytes: avatar.raw_length,
            }))
        }
        Body::Equip(request) => {
            resource::operation_id(&request.operation_id)?;
            if !request.clear && request.asset_revision == 0 {
                return Err(ErrorCode::InvalidMessage);
            }
            let previous = state.storage.equipped(scope.owner)?.revision;
            let result = state.storage.equip(scope.owner, &request)?;
            if result.revision > previous {
                equipment::changed(state, scope.owner).await;
            }
            Ok(Body::Equipped(result))
        }
        Body::Delete(request) => {
            resource::operation_id(&request.operation_id)?;
            if request.expected_revision == 0 {
                return Err(ErrorCode::InvalidMessage);
            }
            let previous = state.storage.equipped(scope.owner)?.revision;
            let result = state.storage.delete(scope.owner, &request)?;
            if result.revision > previous {
                equipment::deleted(state, scope.owner).await;
            }
            Ok(Body::Ack(result))
        }
        Body::FiguraPing(mut ping) => {
            if !ping.owner.is_empty() && ping.owner != scope.owner.to_string() {
                return Err(ErrorCode::Forbidden);
            }
            if ping.arguments.len() > 1024 {
                return Err(ErrorCode::LimitExceeded);
            }
            ping.owner = scope.owner.to_string();
            let _ = state
                .subscribes
                .publisher(scope.owner)
                .send(CloudEvent::FiguraPing {
                    owner: scope.owner,
                    function_id: ping.function_id as u32,
                    echo: ping.echo,
                    arguments: ping.arguments.clone(),
                });
            if ping.echo {
                Ok(Body::FiguraPing(ping))
            } else {
                Ok(Body::Ack(Ack::default()))
            }
        }
        body @ (Body::YsmSnapshot(_) | Body::YsmDelta(_) | Body::YsmEvent(_)) => {
            let publisher = crate::service::ysm::publisher_key(
                &state.protocol_sessions.instance_id,
                &scope.id,
                scope.epoch,
            );
            let now = crate::storage::now_seconds();
            let published = state
                .storage
                .publish_ysm(scope.owner, &publisher, &body, now)?;
            if published.changed {
                let _ = state
                    .subscribes
                    .publisher(scope.owner)
                    .send(CloudEvent::Ysm {
                        owner: scope.owner,
                        expires_at: matches!(&body, Body::YsmEvent(_))
                            .then_some(now.saturating_add(30)),
                        body,
                    });
            }
            Ok(Body::Ack(published.ack))
        }
        Body::SnapshotRequest(request) => {
            let owner = crate::service::ysm::player_owner(&request.owner)?;
            match state
                .storage
                .ysm_snapshot(owner, crate::storage::now_seconds())
            {
                Ok(snapshot) => Ok(Body::YsmSnapshot(snapshot)),
                Err(code) => {
                    crate::service::event::request_snapshot(state, owner, code);
                    Err(code)
                }
            }
        }
        Body::Heartbeat(heartbeat) => Ok(Body::Heartbeat(heartbeat)),
        _ => Err(ErrorCode::InvalidMessage),
    }
}
