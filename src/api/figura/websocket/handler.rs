use anyhow::bail;
use axum::extract::{
    ws::{Message, WebSocket},
    State,
};
use dashmap::DashMap;
use tokio::sync::broadcast;

use crate::service::event::CloudEvent;
use crate::{auth::Userinfo, AppState};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::{
    processor::*, AuthModeError, C2SMessage, RADError, S2CMessage, SessionMessage, WSSession,
};

pub async fn initial(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let supports_ysm = headers
        .get("x-omnimorph-models")
        .and_then(|value| value.to_str().ok())
        == Some("ysm");
    ws.max_message_size(1030)
        .max_frame_size(1030)
        .on_upgrade(move |socket| handle_socket(socket, state, supports_ysm))
}

async fn handle_socket(mut ws: WebSocket, state: AppState, supports_ysm: bool) {
    // Trying authenticate & get user data or dropping connection
    match authenticate(&mut ws, &state).await {
        Ok(user) => {
            // Creating session & creating/getting channels
            let session_id;
            let mut session = {
                let sub_workers_aborthandles = DashMap::new();

                // Channel for receiving messages from internal functions.
                let (own_tx, own_rx) = crate::service::queue::channel(32);
                session_id = match state.session.register_bounded(user.uuid, own_tx.clone()) {
                    Ok(id) => id,
                    Err(_) => {
                        let _ = ws.close().await;
                        return;
                    }
                };

                // Channel for sending messages to subscribers
                let subs_tx = state.subscribes.publisher(user.uuid);

                WSSession {
                    user: user.clone(),
                    own_tx,
                    own_rx,
                    subs_tx,
                    sub_workers_aborthandles,
                }
            };

            // Starting main worker
            let (_, shutdown) = state
                .session
                .close_signal(session_id)
                .expect("registered connection");
            match main_worker(
                &mut session,
                &mut ws,
                &state,
                shutdown,
                session_id,
                supports_ysm,
            )
            .await
            {
                Ok(_) => (),
                Err(kind) => tracing::error!("[WebSocket] Main worker halted due to {}.", kind),
            }

            for (_, handle) in session.sub_workers_aborthandles {
                handle.abort();
            }

            // Removing session data
            state.session.remove(session_id);
        }
        Err(kind) => {
            tracing::info!("[WebSocket] Can't authenticate: {}", kind);
        }
    }

    // Closing connection
    if let Err(kind) = ws.close().await {
        tracing::trace!("[WebSocket] Closing fault: {}", kind)
    }
}

async fn main_worker(
    session: &mut WSSession,
    ws: &mut WebSocket,
    state: &AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    connection_id: u64,
    supports_ysm: bool,
) -> anyhow::Result<()> {
    tracing::debug!(
        "WebSocket control for {} is transferred to the main worker",
        session.user.nickname
    );
    let mut ysm_fragments = crate::service::ysm::legacy::Reassembly::default();
    let supports_ysm = Arc::new(AtomicBool::new(supports_ysm));
    let mut outbound_message_id = 0u64;
    let publisher = crate::service::ysm::publisher_key(
        &state.protocol_sessions.instance_id,
        &format!("figura:{connection_id}"),
        state.protocol_sessions.server_epoch,
    );
    loop {
        if *shutdown.borrow() {
            bail!("slow consumer requires reconnect");
        }
        tokio::select! {
            _ = shutdown.changed() => bail!("slow consumer requires reconnect"),
            external_msg = ws.recv_and_decode() => {

                // Getting a value or halt the worker without an error
                let external_msg = match external_msg {
                    Ok(m) => m,
                    Err(kind) => {
                        match kind {
                            RADError::Close(_) => return Ok(()),
                            RADError::StreamClosed => return Ok(()),
                            _ => return Err(kind.into())
                        }
                    },
                };

                // Processing message
                match external_msg {
                    C2SMessage::Token(_) => bail!("authentication passed, but the client sent the Token again"),
                    C2SMessage::Ping(func_id, echo, data) => {
                        if data.len() > 1024 || state.protocol_sessions.charge(session.user.uuid, data.len() + 6).is_err() {
                            ws.send(Message::Binary(S2CMessage::Notice(1).into())).await?;
                            continue;
                        }
                        if crate::service::ysm::legacy::recognizes(func_id, &data) {
                            let received = ysm_fragments.accept(session.user.uuid, &data)
                                .map_err(|code| anyhow::anyhow!("M7 fragment rejected: {code:?}"))?;
                            supports_ysm.store(true, Ordering::Release);
                            if let Some(bytes) = received {
                                let now = crate::storage::now_seconds();
                                let messages = state.storage.publish_legacy_ysm(session.user.uuid, &publisher, &bytes, now)
                                    .map_err(|code| anyhow::anyhow!("M7 publication rejected: {code:?}"))?;
                                for body in messages {
                                    let expires_at = matches!(&body, crate::protocol::v1::envelope::Body::YsmEvent(_))
                                        .then_some(now.saturating_add(30));
                                    let _ = session.subs_tx.send(CloudEvent::Ysm {
                                        owner: session.user.uuid, body, expires_at,
                                    });
                                }
                            }
                            if echo {
                                ws.send(Message::Binary(S2CMessage::Ping(session.user.uuid, func_id, true, data).into())).await?;
                            }
                            continue;
                        }
                        let event = CloudEvent::FiguraPing {
                            owner: session.user.uuid,
                            function_id: func_id,
                            echo,
                            arguments: data,
                        };

                        // Echo check
                        if echo {
                            if let Some(bytes) = encode_event(event.clone()) {
                                ws.send(Message::Binary(bytes)).await?
                            }
                        }
                        // Sending to others
                        let _ = session.subs_tx.send(event);
                    },
                    C2SMessage::Sub(uuid) => {
                        tracing::debug!("[WebSocket] {} subscribes to {}", session.user.nickname, uuid);

                        // Doesn't allow to subscribe to yourself
                        if session.user.uuid != uuid
                            && !session.sub_workers_aborthandles.contains_key(&uuid)
                        {
                            if session.sub_workers_aborthandles.len() >= 128 {
                                ws.send(Message::Binary(S2CMessage::Notice(1).into())).await?;
                                continue;
                            }
                            // Creates a channel to send pings to a subscriber if it can't find an existing one
                            let rx = state.subscribes.subscribe(uuid);
                            let handle = tokio::spawn(sub_worker(session.own_tx.clone(), rx, state.clone(), uuid, supports_ysm.clone())).abort_handle();
                            session.sub_workers_aborthandles.insert(uuid, handle);
                        }
                    },
                    C2SMessage::Unsub(uuid) => {
                        tracing::debug!("[WebSocket] {} unsubscribes from {}", session.user.nickname, uuid);

                        match session.sub_workers_aborthandles.remove(&uuid) {
                            Some((_, handle)) => handle.abort(),
                            None => tracing::warn!("[WebSocket] {} was not subscribed.", session.user.nickname),
                        };
                    },
                }
            },
            internal_msg = session.own_rx.recv() => {
                let internal_msg = internal_msg.ok_or(anyhow::anyhow!("Unexpected error! Session channel broken!"))?;
                match internal_msg {
                    SessionMessage::Event(event) => {
                        forward_event(ws, event, supports_ysm.load(Ordering::Acquire), &mut outbound_message_id).await?;
                    },
                    SessionMessage::Ping(msg) => {
                        ws.send(Message::Binary(msg)).await?
                    },
                    SessionMessage::Banned => {
                        let _ = ban_action(ws).await
                            .inspect_err(
                                |kind| tracing::warn!("[WebSocket] Didn't get the ban message due to {}", kind)
                            );
                        bail!("{} banned!", session.user.nickname)
                    },
                    SessionMessage::ResyncRequired => bail!("expired YSM event requires recovery"),
                }
            }
        }
    }
}

async fn sub_worker(
    tx_main: crate::service::queue::Sender,
    mut rx: broadcast::Receiver<CloudEvent>,
    state: AppState,
    owner: uuid::Uuid,
    supports_ysm: Arc<AtomicBool>,
) {
    let mut baseline = None;
    if supports_ysm.load(Ordering::Acquire) {
        if let Ok(snapshot) = state
            .storage
            .ysm_snapshot(owner, crate::storage::now_seconds())
        {
            baseline = snapshot.identity.clone();
            if tx_main
                .try_send(SessionMessage::Event(CloudEvent::Ysm {
                    owner,
                    body: crate::protocol::v1::envelope::Body::YsmSnapshot(snapshot),
                    expires_at: None,
                }))
                .is_err()
            {
                return;
            }
        }
    }
    loop {
        let msg = match rx.recv().await {
            Ok(m) => m,
            Err(kind) => {
                tracing::error!("[Subscribes_Worker] Broadcast error! {}", kind);
                tx_main.close_signal().send_replace(true);
                return;
            }
        };
        match &msg {
            CloudEvent::Ysm { body, .. } => {
                if !supports_ysm.load(Ordering::Acquire) {
                    continue;
                }
                // A legacy publisher can announce M7 support after subscribing.
                // Establish a watermarked baseline before forwarding its next delta.
                if baseline.is_none() {
                    if let Ok(snapshot) = state
                        .storage
                        .ysm_snapshot(owner, crate::storage::now_seconds())
                    {
                        baseline = snapshot.identity.clone();
                        if tx_main
                            .try_send(SessionMessage::Event(CloudEvent::Ysm {
                                owner,
                                body: crate::protocol::v1::envelope::Body::YsmSnapshot(snapshot),
                                expires_at: None,
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                match crate::service::ysm::advance_cursor(&mut baseline, body) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(_) => {
                        tx_main.close_signal().send_replace(true);
                        return;
                    }
                }
            }
            CloudEvent::AvatarChanged { .. } => baseline = None,
            _ => {}
        }
        match tx_main.send(SessionMessage::Event(msg)).await {
            Ok(_) => (),
            Err(kind) => {
                tracing::error!("[Subscribes_Worker] Session error! {}", kind);
                return;
            }
        }
    }
}

async fn forward_event(
    ws: &mut WebSocket,
    event: CloudEvent,
    supports_ysm: bool,
    message_id: &mut u64,
) -> anyhow::Result<()> {
    let packets = match event {
        CloudEvent::Ysm {
            owner,
            body,
            expires_at,
        } => {
            if !supports_ysm {
                return Ok(());
            }
            if expires_at.is_some_and(|deadline| deadline <= crate::storage::now_seconds()) {
                bail!("expired M7 event requires recovery");
            }
            let Some(bytes) = crate::service::ysm::legacy::encode_body(&body)
                .map_err(|code| anyhow::anyhow!("M7 encoding rejected: {code:?}"))?
            else {
                return Ok(());
            };
            *message_id = message_id
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("M7 message ID exhausted"))?;
            crate::service::ysm::legacy::fragment(*message_id, &bytes)
                .map_err(|code| anyhow::anyhow!("M7 framing rejected: {code:?}"))?
                .into_iter()
                .map(|packet| {
                    S2CMessage::Ping(owner, crate::service::ysm::legacy::PING_ID, false, packet)
                        .into()
                })
                .collect()
        }
        event => encode_event(event).into_iter().collect::<Vec<Vec<u8>>>(),
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for bytes in packets {
            ws.send(Message::Binary(bytes)).await?;
        }
        Ok::<(), axum::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("slow M7 consumer requires reconnect"))??;
    Ok(())
}

fn encode_event(event: CloudEvent) -> Option<Vec<u8>> {
    match event {
        CloudEvent::FiguraPing {
            owner,
            function_id,
            echo,
            arguments,
        } => Some(S2CMessage::Ping(owner, function_id, echo, arguments).into()),
        CloudEvent::AvatarChanged { owner, .. } => Some(S2CMessage::Event(owner).into()),
        CloudEvent::LegacyRaw(bytes) => Some(bytes),
        // Typed YSM is handled by forward_event and encoded as M7, never protobuf.
        CloudEvent::Ysm { .. } => None,
    }
}

async fn authenticate(socket: &mut WebSocket, state: &AppState) -> Result<Userinfo, AuthModeError> {
    match socket.recv_and_decode().await {
        Ok(msg) => {
            match msg {
                C2SMessage::Token(token) => {
                    let token = String::from_utf8(token.to_vec()).map_err(|_| AuthModeError::ConvertError)?;
                    let identity = state.user_manager.get(&token).map(|user| user.clone());
                    match identity {
                        Some(user) => {
                            if socket.send(Message::Binary(S2CMessage::Auth.into())).await.is_err() {
                                Err(AuthModeError::SendError)
                            } else if !user.banned {
                                Ok(user.clone())
                            } else {
                                let _ = ban_action(socket).await
                                    .inspect_err(
                                        |kind| tracing::warn!("[WebSocket] Didn't get the ban message due to {}", kind)
                                    );
                                Err(AuthModeError::Banned(user.nickname.clone()))
                            }
                        },
                        None => {
                            if socket.send(
                                Message::Close(Some(axum::extract::ws::CloseFrame { code: 4000, reason: "Re-auth".into() }))
                            ).await.is_err() {
                                Err(AuthModeError::SendError)
                            } else {
                                Err(AuthModeError::AuthenticationFailure)
                            }
                        },
                    }
                },
                _ => {
                    Err(AuthModeError::UnauthorizedAction)
                }
            }
        },
        Err(err) => {
            Err(AuthModeError::RecvError(err))
        },
    }
}

async fn ban_action(ws: &mut WebSocket) -> anyhow::Result<()> {
    ws.send(Message::Binary(S2CMessage::Toast(2, "You're banned!".to_string(), None).into())).await?;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    ws.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 4001, reason: "You're banned!".into() }))).await?;

    Ok(())
}
