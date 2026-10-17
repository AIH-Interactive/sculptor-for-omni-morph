use anyhow::bail;
use axum::extract::{ws::{Message, WebSocket}, State};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};

use crate::{auth::Userinfo, AppState};
use crate::service::event::CloudEvent;

use super::{processor::*, AuthModeError, S2CMessage, C2SMessage, WSSession, SessionMessage, RADError};

pub async fn initial(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<AppState>
) -> axum::response::Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut ws: WebSocket, state: AppState) {
    // Trying authenticate & get user data or dropping connection
    match authenticate(&mut ws, &state).await {
        Ok(user) => {

            // Creating session & creating/getting channels
            let session_id;
            let mut session = {
                let sub_workers_aborthandles = DashMap::new();
                
                // Channel for receiving messages from internal functions.
                let (own_tx, own_rx) = mpsc::channel(32);
                session_id = match state.session.register_bounded(user.uuid, own_tx.clone()) {
                    Ok(id) => id,
                    Err(_) => { let _ = ws.close().await; return; }
                };

                // Channel for sending messages to subscribers
                let subs_tx = state.subscribes.publisher(user.uuid);

                WSSession { user: user.clone(), own_tx, own_rx, subs_tx, sub_workers_aborthandles }
            };

            // Starting main worker
            let (_, shutdown) = state.session.close_signal(session_id).expect("registered connection");
            match main_worker(&mut session, &mut ws, &state, shutdown).await {
                Ok(_) => (),
                Err(kind) => tracing::error!("[WebSocket] Main worker halted due to {}.",  kind),
            }

            for (_, handle) in session.sub_workers_aborthandles {
                handle.abort();
            }
        
            // Removing session data
            state.session.remove(session_id);
        },
        Err(kind) => {
            tracing::info!("[WebSocket] Can't authenticate: {}", kind);
        }
    }

    // Closing connection
    if let Err(kind) = ws.close().await { tracing::trace!("[WebSocket] Closing fault: {}", kind) }
}

async fn main_worker(session: &mut WSSession, ws: &mut WebSocket, state: &AppState,
                     mut shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
    tracing::debug!("WebSocket control for {} is transferred to the main worker", session.user.nickname);
    loop {
        if *shutdown.borrow() { bail!("slow consumer requires reconnect"); }
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
                        let event = CloudEvent::FiguraPing {
                            owner: session.user.uuid,
                            function_id: func_id,
                            echo,
                            arguments: data,
                        };
                        
                        // Echo check
                        if echo {
                            ws.send(Message::Binary(encode_event(event.clone()))).await?
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
                            let handle = tokio::spawn(sub_worker(session.own_tx.clone(), rx)).abort_handle();
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
                        ws.send(Message::Binary(encode_event(event))).await?
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
                }
            }
        }
    }
}

async fn sub_worker(tx_main: mpsc::Sender<SessionMessage>, mut rx: broadcast::Receiver<CloudEvent>) {
    loop {
        let msg = match rx.recv().await {
            Ok(m) => m,
            Err(kind) => {
                tracing::error!("[Subscribes_Worker] Broadcast error! {}", kind);
                return;
            },
        };
        match tx_main.send(SessionMessage::Event(msg)).await {
            Ok(_) => (),
            Err(kind) => {
                tracing::error!("[Subscribes_Worker] Session error! {}", kind);
                return;
            },
        }
    }
}

fn encode_event(event: CloudEvent) -> Vec<u8> {
    match event {
        CloudEvent::FiguraPing {
            owner,
            function_id,
            echo,
            arguments,
        } => {
            S2CMessage::Ping(owner, function_id, echo, arguments).into()
        }
        CloudEvent::AvatarChanged { owner, .. } => S2CMessage::Event(owner).into(),
        CloudEvent::LegacyRaw(bytes) => bytes,
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
