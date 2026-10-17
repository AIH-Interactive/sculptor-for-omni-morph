use super::*;
use crate::{api::figura::SessionMessage, service::event::CloudEvent};
use axum::extract::{
    ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    OriginalUri, State,
};
use std::{collections::HashMap, time::Duration};
use tokio::{task::AbortHandle, time::Instant};
use uuid::Uuid;

pub async fn upgrade(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    if uri.query().is_some()
        || !headers
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(',').any(|name| name.trim() == SUBPROTOCOL))
            .unwrap_or(false)
    {
        return failure(ErrorCode::InvalidMessage, 0, None);
    }
    let scope = match file_scope(&state, &headers) {
        Ok(scope) => scope,
        Err(code) => return failure(code, 0, None),
    };
    websocket
        .protocols([SUBPROTOCOL])
        .max_message_size(wire::MAX_ENVELOPE_BYTES)
        .max_frame_size(wire::MAX_ENVELOPE_BYTES)
        .on_upgrade(move |socket| serve(socket, state, scope))
}

async fn send(socket: &mut WebSocket, message: Envelope) -> Result<(), ErrorCode> {
    if message.encoded_len() > wire::MAX_ENVELOPE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(WsMessage::Binary(message.encode_to_vec())),
    )
    .await
    .map_err(|_| ErrorCode::Offline)?
    .map_err(|_| ErrorCode::Offline)
}

fn decode(scope: &ScopedSession, bytes: &[u8]) -> Result<Envelope, ErrorCode> {
    let message = wire::decode(bytes)?;
    if message.session_id != scope.id || message.connection_epoch != scope.epoch {
        return Err(ErrorCode::Unauthenticated);
    }
    Ok(message)
}

async fn serve(mut socket: WebSocket, state: AppState, scope: ScopedSession) {
    let (sender, mut receiver) = crate::service::queue::channel(32);
    let mut subscriptions = HashMap::new();
    let mut registered = None;
    let mut attached = false;
    let result = async {
        let first = tokio::time::timeout(Duration::from_secs(10), socket.recv())
            .await
            .map_err(|_| ErrorCode::Offline)?
            .ok_or(ErrorCode::Offline)?
            .map_err(|_| ErrorCode::InvalidMessage)?;
        let WsMessage::Binary(bytes) = first else {
            return Err(ErrorCode::InvalidMessage);
        };
        let request = decode(&scope, &bytes)?;
        let Some(envelope::Body::Hello(hello)) = request.body else {
            return Err(ErrorCode::InvalidMessage);
        };
        if !hello.supported_majors.contains(&1) {
            return Err(ErrorCode::UnsupportedVersion);
        }
        let supports_ysm = hello.capabilities.iter().any(|capability| matches!(Capability::try_from(*capability), Ok(Capability::YsmPlain | Capability::YsmEncrypted | Capability::YsmRecovery)));
        state.protocol_sessions.authorize(&state, &scope.id, scope.epoch)?;
        state.protocol_sessions.attach(&scope.id)?;
        attached = true;
        let id = state.session.register_bounded(scope.owner, sender.clone())?;
        registered = Some(id);
        let (_close, mut shutdown) = state.session.close_signal(id).ok_or(ErrorCode::Internal)?;
        send(&mut socket, envelope(request.request_id, Some(&scope), envelope::Body::HelloAck(hello_ack(&state).await))).await?;
        let mut last_seen = Instant::now();
        let mut heartbeat = tokio::time::interval_at(Instant::now() + Duration::from_secs(20), Duration::from_secs(20));
        loop {
            if *shutdown.borrow() {
                return Err(ErrorCode::SnapshotRequired);
            }
            tokio::select! {
                incoming = socket.recv() => {
                    let Some(incoming) = incoming else {
                        return Ok(());
                    };
                    match incoming.map_err(|_| ErrorCode::InvalidMessage)? {
                        WsMessage::Close(_) => return Ok(()),
                        WsMessage::Ping(bytes) => {
                            tokio::time::timeout(Duration::from_secs(5), socket.send(WsMessage::Pong(bytes)))
                                .await
                                .map_err(|_| ErrorCode::Offline)?
                                .map_err(|_| ErrorCode::Offline)?;
                        }
                        WsMessage::Pong(_) => last_seen = Instant::now(),
                        WsMessage::Binary(bytes) => {
                            let request = match decode(&scope, &bytes) {
                                Ok(request) => request,
                                Err(code) => {
                                    send(&mut socket, envelope(0, Some(&scope), error_body(code, 0))).await?;
                                    return Ok(());
                                }
                            };
                            last_seen = Instant::now();
                            state.protocol_sessions.authorize(&state, &scope.id, scope.epoch)?;
                            if let Err(code) = state.protocol_sessions.charge(scope.owner, bytes.len()) {
                                send(&mut socket, envelope(request.request_id, Some(&scope), error_body(code, request.request_id))).await?;
                                continue;
                            }
                            let body = handle(&state, &scope, request.body.unwrap(), &sender, supports_ysm, &mut subscriptions).await;
                            match body {
                                Ok(Some(body)) => send(&mut socket, envelope(request.request_id, Some(&scope), body)).await?,
                                Ok(None) => {}
                                Err(code) => send(&mut socket, envelope(request.request_id, Some(&scope), error_body(code, request.request_id))).await?,
                            }
                        }
                        _ => return Err(ErrorCode::InvalidMessage),
                    }
                }
                outgoing = receiver.recv() => {
                    match outgoing.ok_or(ErrorCode::Offline)? {
                        SessionMessage::Event(event) => {
                            state.protocol_sessions.authorize(&state, &scope.id, scope.epoch)?;
                            if !supports_ysm && matches!(&event, CloudEvent::Ysm { .. }) { continue; }
                            if let Some(body) = event_body(&state, scope.owner, event).await? {
                                send(&mut socket, envelope(0, Some(&scope), body)).await?;
                            }
                        }
                        SessionMessage::Banned => return Err(ErrorCode::Forbidden),
                        SessionMessage::ResyncRequired => return Err(ErrorCode::SnapshotRequired),
                        SessionMessage::Ping(_) => {} // Privileged legacy bytes never enter this adapter.
                    }
                }
                _ = shutdown.changed() => return Err(ErrorCode::SnapshotRequired),
                _ = heartbeat.tick() => {
                    state.protocol_sessions.authorize(&state, &scope.id, scope.epoch)?;
                    if last_seen.elapsed() >= Duration::from_secs(60) {
                        return Err(ErrorCode::Offline);
                    }
                    send(&mut socket, envelope(0, Some(&scope), envelope::Body::Heartbeat(Heartbeat { nonce: crate::storage::now_seconds() }))).await?;
                }
            }
        }
    }.await;
    if let Err(code) = result {
        let _ = send(&mut socket, envelope(0, Some(&scope), error_body(code, 0))).await;
    }
    for (_, worker) in subscriptions {
        worker.abort();
    }
    if let Some(id) = registered {
        state.session.remove(id);
    }
    if attached {
        state.protocol_sessions.detach(&scope.id);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
}

async fn handle(
    state: &AppState,
    scope: &ScopedSession,
    body: envelope::Body,
    sender: &crate::service::queue::Sender,
    supports_ysm: bool,
    subscriptions: &mut HashMap<Uuid, AbortHandle>,
) -> Result<Option<envelope::Body>, ErrorCode> {
    use envelope::Body;
    match body {
        Body::Subscribe(request) => {
            if !supports_ysm && !request.cursors.is_empty() {
                return Err(ErrorCode::UnsupportedCapability);
            }
            let owners: std::collections::HashSet<Uuid> = request
                .owners
                .iter()
                .map(|owner| Uuid::parse_str(owner).map_err(|_| ErrorCode::InvalidMessage))
                .collect::<Result<_, _>>()?;
            let mut cursors = HashMap::new();
            for cursor in request.cursors {
                let owner =
                    Uuid::parse_str(&cursor.owner).map_err(|_| ErrorCode::InvalidMessage)?;
                if !owners.contains(&owner) || cursors.insert(owner, cursor).is_some() {
                    return Err(ErrorCode::InvalidMessage);
                }
            }
            if subscriptions
                .keys()
                .copied()
                .chain(owners.iter().copied())
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 128
            {
                return Err(ErrorCode::LimitExceeded);
            }
            for owner in owners {
                if owner == scope.owner || subscriptions.contains_key(&owner) {
                    continue;
                }
                let worker = subscription::spawn(
                    state.clone(),
                    owner,
                    scope.owner,
                    cursors.remove(&owner),
                    sender.clone(),
                    supports_ysm,
                );
                subscriptions.insert(owner, worker);
            }
            Ok(Some(Body::Ack(Ack::default())))
        }
        Body::Resume(cursor) => {
            if !supports_ysm {
                return Err(ErrorCode::UnsupportedCapability);
            }
            let owner = Uuid::parse_str(&cursor.owner).map_err(|_| ErrorCode::InvalidMessage)?;
            if owner == scope.owner {
                return Err(ErrorCode::InvalidMessage);
            }
            if !subscriptions.contains_key(&owner) && subscriptions.len() >= 128 {
                return Err(ErrorCode::LimitExceeded);
            }
            if let Some(worker) = subscriptions.remove(&owner) {
                worker.abort();
            }
            let worker = subscription::spawn(
                state.clone(),
                owner,
                scope.owner,
                Some(cursor),
                sender.clone(),
                supports_ysm,
            );
            subscriptions.insert(owner, worker);
            Ok(Some(Body::Ack(Ack::default())))
        }
        Body::Unsubscribe(request) => {
            let owners = request
                .owners
                .iter()
                .map(|owner| Uuid::parse_str(owner).map_err(|_| ErrorCode::InvalidMessage))
                .collect::<Result<Vec<_>, _>>()?;
            for owner in owners {
                if let Some(worker) = subscriptions.remove(&owner) {
                    worker.abort();
                }
            }
            Ok(Some(Body::Ack(Ack::default())))
        }
        Body::Heartbeat(_) => Ok(None),
        body => http::dispatch(state, scope, body).await.map(Some),
    }
}

async fn event_body(
    state: &AppState,
    viewer: Uuid,
    event: CloudEvent,
) -> Result<Option<envelope::Body>, ErrorCode> {
    if !crate::service::library::event_visible(state, viewer, &event).await? {
        return Ok(None);
    }
    use envelope::Body;
    match event {
        CloudEvent::FiguraPing {
            owner,
            function_id,
            echo,
            arguments,
        } => Ok(Some(Body::FiguraPing(FiguraPing {
            owner: owner.to_string(),
            function_id: function_id as i32,
            echo,
            arguments,
        }))),
        CloudEvent::AvatarChanged { owner, tombstone } => {
            let storage = state.storage.clone();
            let equipped = crate::service::library::work(move || {
                storage.library_project_equipped(owner, viewer)
            })
            .await?;
            let tombstone = tombstone
                || (owner != viewer
                    && equipped.primary.is_none()
                    && equipped.figura_compatibility.is_none());
            Ok(Some(Body::AvatarChanged(AvatarChanged {
                equipped: Some(equipped),
                tombstone,
            })))
        }
        CloudEvent::LegacyRaw(_) => Ok(None),
        CloudEvent::Ysm { body, .. } => Ok(Some(body)),
    }
}
