//! Independent revision notifications. Persistent state is read through authorized HTTP snapshots.
use super::*;
use crate::{api::figura::SessionMessage, service::event::CloudEvent};
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    OriginalUri, State,
};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};
use tokio::{task::AbortHandle, time::Instant};

pub(crate) const SUBPROTOCOL: &str = "omnimorph.outfit.v1";
const MAX_MESSAGE_BYTES: usize = 16_384;
const MAX_SUBSCRIPTIONS: usize = 128;

fn recovery_required() -> LibraryError {
    LibraryError {
        status: StatusCode::CONFLICT,
        code: "SNAPSHOT_REQUIRED",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    schema_version: u32,
    connection_epoch: u64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    owners: Vec<Uuid>,
}

fn decode(bytes: &[u8], epoch: Option<u64>) -> Result<Command> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ErrorCode::LimitExceeded.into());
    }
    let command: Command = serde_json::from_slice(bytes).map_err(|_| ErrorCode::InvalidMessage)?;
    if command.schema_version != 1
        || command.connection_epoch == 0
        || command.connection_epoch > i64::MAX as u64
        || epoch.is_some_and(|epoch| epoch != command.connection_epoch)
        || command.owners.len() > MAX_SUBSCRIPTIONS
        || command
            .owners
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != command.owners.len()
    {
        return Err(ErrorCode::InvalidMessage.into());
    }
    Ok(command)
}

pub async fn upgrade(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Result<Response> {
    if uri.query().is_some()
        || headers.get_all("sec-websocket-protocol").iter().count() != 1
        || !headers["sec-websocket-protocol"]
            .to_str()
            .map(|value| value.split(',').any(|name| name.trim() == SUBPROTOCOL))
            .unwrap_or(false)
    {
        return Err(ErrorCode::InvalidMessage.into());
    }
    let viewer = player_identity(&state, &headers)?;
    let origin = origin(&headers)?;
    Ok(websocket
        .protocols([SUBPROTOCOL])
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(socket, state, headers, viewer, origin)))
}

struct Channel {
    socket: WebSocket,
    state: AppState,
    headers: HeaderMap,
    viewer: Uuid,
    origin: String,
    epoch: u64,
    sequence: u64,
}

impl Channel {
    fn authorize(&self) -> Result<()> {
        if player_identity(&self.state, &self.headers)? != self.viewer {
            return Err(ErrorCode::Unauthenticated.into());
        }
        Ok(())
    }

    async fn send(&mut self, mut body: Value) -> Result<()> {
        self.authorize()?;
        self.sequence = self.sequence.checked_add(1).ok_or(ErrorCode::Internal)?;
        body["schema_version"] = json!(1);
        body["connection_epoch"] = json!(self.epoch);
        // The existing random u64 server epoch is opaque, not a Java signed long.
        body["server_epoch"] = json!(self.state.protocol_sessions.server_epoch.to_string());
        body["sequence"] = json!(self.sequence);
        let bytes = serde_json::to_string(&body).map_err(|_| ErrorCode::Internal)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(ErrorCode::LimitExceeded.into());
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            self.socket.send(Message::Text(bytes)),
        )
        .await
        .map_err(|_| ErrorCode::Offline)?
        .map_err(|_| ErrorCode::Offline)?;
        Ok(())
    }

    async fn changed(&mut self, owner: Uuid) -> Result<()> {
        self.authorize()?;
        let storage = self.state.storage.clone();
        let origin = self.origin.clone();
        let viewer = self.viewer;
        let limits = self.state.config.read().await.outfit.limits.clone();
        let snapshot =
            blocking(move || storage.outfit_project(owner, viewer, &origin, &limits)).await?;
        // Never forward names, references, scripts or private source bodies in notifications.
        self.send(json!({"type": "CHANGED", "owner": owner,
            "revision": snapshot.revision, "equipment_revision": snapshot.equipment_revision}))
            .await
    }

    async fn state_changed(&mut self, owner: Uuid, instance: Uuid) -> Result<()> {
        self.authorize()?;
        let storage = self.state.storage.clone();
        let origin = self.origin.clone();
        let viewer = self.viewer;
        let limits = self.state.config.read().await.outfit.limits.clone();
        let state = blocking(move || storage.outfit_instance_state(owner, viewer, &origin, instance, &limits, None)).await;
        match state {
            Ok(state) => self.send(json!({"type": "STATE_CHANGED", "owner": owner,
                "instance_id": instance, "outfit_revision": state.outfit_revision, "state_revision": state.revision})).await,
            Err(error) if matches!(error.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn subscribe(state: &AppState, owner: Uuid, sender: crate::service::queue::Sender) -> AbortHandle {
    let mut events = state.subscribes.subscribe(owner);
    tokio::spawn(async move {
        loop {
            let message = match events.recv().await {
                Ok(event @ (CloudEvent::AvatarChanged { .. } | CloudEvent::OutfitStateChanged { .. })) => SessionMessage::Event(event),
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    sender.close_signal().send_replace(true);
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if sender.try_send(message).is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

async fn serve(
    socket: WebSocket,
    state: AppState,
    headers: HeaderMap,
    viewer: Uuid,
    origin: String,
) {
    let mut channel = Channel {
        socket,
        state,
        headers,
        viewer,
        origin,
        epoch: 0,
        sequence: 0,
    };
    let (sender, mut receiver) = crate::service::queue::channel(32);
    let mut subscriptions: HashMap<Uuid, AbortHandle> = HashMap::new();
    let mut registered = None;
    let result: Result<()> = async {
        let first = tokio::time::timeout(Duration::from_secs(10), channel.socket.recv())
            .await
            .map_err(|_| ErrorCode::Offline)?
            .ok_or(ErrorCode::Offline)?
            .map_err(|_| ErrorCode::InvalidMessage)?;
        let Message::Text(bytes) = first else {
            return Err(ErrorCode::InvalidMessage.into());
        };
        channel.authorize()?;
        channel.state.protocol_sessions.charge(viewer, bytes.len())?;
        let hello = decode(bytes.as_bytes(), None)?;
        if hello.kind != "HELLO" || !hello.owners.is_empty() {
            return Err(ErrorCode::InvalidMessage.into());
        }
        if let Some(epoch) = channel.headers.get("x-omnimorph-epoch") {
            if epoch.to_str().ok().and_then(|value| value.parse::<u64>().ok())
                != Some(hello.connection_epoch)
            {
                return Err(ErrorCode::Unauthenticated.into());
            }
        }
        channel.epoch = hello.connection_epoch;
        let id = channel.state.session.register_bounded(viewer, sender.clone())?;
        registered = Some(id);
        let (_, mut shutdown) = channel.state.session.close_signal(id).ok_or(ErrorCode::Internal)?;
        channel.send(json!({
            "type": "HELLO_ACK",
            "instance_id": channel.state.protocol_sessions.instance_id,
        })).await?;
        let mut last_seen = Instant::now();
        let mut heartbeat = tokio::time::interval_at(
            Instant::now() + Duration::from_secs(20), Duration::from_secs(20));
        let mut limits = channel.state.config.read().await.outfit.limits.clone();
        loop {
            if *shutdown.borrow() {
                return Err(recovery_required());
            }
            tokio::select! {
                incoming = channel.socket.recv() => {
                    let Some(incoming) = incoming else {
                        return Ok(());
                    };
                    match incoming.map_err(|_| ErrorCode::InvalidMessage)? {
                        Message::Close(_) => return Ok(()),
                        Message::Text(bytes) => {
                            channel.authorize()?;
                            channel.state.protocol_sessions.charge(viewer, bytes.len())?;
                            let command = decode(bytes.as_bytes(), Some(channel.epoch))?;
                            last_seen = Instant::now();
                            match command.kind.as_str() {
                                "SUBSCRIBE" => {
                                    let additional = command.owners.iter()
                                        .filter(|owner| !subscriptions.contains_key(owner))
                                        .count();
                                    if subscriptions.len() + additional > MAX_SUBSCRIPTIONS {
                                        return Err(ErrorCode::LimitExceeded.into());
                                    }
                                    for owner in &command.owners {
                                        subscriptions.entry(*owner).or_insert_with(|| {
                                            subscribe(&channel.state, *owner, sender.clone())
                                        });
                                    }
                                    channel.send(json!({"type": "ACK", "command": "SUBSCRIBE"})).await?;
                                    // Initial snapshots bypass the 32-item queue, including a full 128-player set.
                                    for owner in command.owners {
                                        channel.changed(owner).await?;
                                    }
                                }
                                "UNSUBSCRIBE" => {
                                    for owner in command.owners {
                                        if let Some(worker) = subscriptions.remove(&owner) {
                                            worker.abort();
                                        }
                                    }
                                    channel.send(json!({"type": "ACK", "command": "UNSUBSCRIBE"})).await?;
                                }
                                "HEARTBEAT" if command.owners.is_empty() => {
                                    channel.send(json!({"type": "HEARTBEAT"})).await?;
                                }
                                _ => return Err(ErrorCode::InvalidMessage.into()),
                            }
                        }
                        Message::Pong(_) => last_seen = Instant::now(),
                        // JSON heartbeat is the bounded, authenticated liveness mechanism.
                        _ => return Err(ErrorCode::InvalidMessage.into()),
                    }
                }
                outgoing = receiver.recv() => {
                    match outgoing.ok_or(ErrorCode::Offline)? {
                        SessionMessage::Event(CloudEvent::AvatarChanged { owner, .. })
                            if owner == viewer || subscriptions.contains_key(&owner) => {
                                channel.changed(owner).await?;
                            }
                        SessionMessage::Event(CloudEvent::OutfitStateChanged { owner, instance })
                            if owner == viewer || subscriptions.contains_key(&owner) => {
                                channel.state_changed(owner, instance).await?;
                            }
                        SessionMessage::Banned => return Err(ErrorCode::Forbidden.into()),
                        SessionMessage::ResyncRequired => return Err(recovery_required()),
                        _ => {} // Legacy bytes, pings and YSM frames never enter this adapter.
                    }
                }
                _ = shutdown.changed() => return Err(recovery_required()),
                _ = heartbeat.tick() => {
                    channel.authorize()?;
                    if last_seen.elapsed() >= Duration::from_secs(60) {
                        return Err(ErrorCode::Offline.into());
                    }
                    let current = channel.state.config.read().await.outfit.limits.clone();
                    if current != limits {
                        limits = current;
                        channel.send(json!({"type": "LIMITS_CHANGED"})).await?;
                    }
                    channel.send(json!({"type": "HEARTBEAT"})).await?;
                }
            }
        }
    }.await;
    if let Err(error) = result {
        let kind = if error.code == "INVALID_REQUEST" {
            "ERROR"
        } else {
            "RESYNC_REQUIRED"
        };
        let _ = channel
            .send(json!({"type": kind, "code": error.code}))
            .await;
    }
    for (_, worker) in subscriptions {
        worker.abort();
    }
    if let Some(id) = registered {
        channel.state.session.remove(id);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), channel.socket.close()).await;
}
