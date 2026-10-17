use super::*;
use crate::{
    api::figura::SessionMessage,
    service::{event::CloudEvent, queue::Sender, ysm},
};
use tokio::task::AbortHandle;
use uuid::Uuid;

fn enqueue(sender: &Sender, owner: Uuid, body: envelope::Body) -> Result<(), ErrorCode> {
    enqueue_with_deadline(sender, owner, body, None)
}

fn enqueue_with_deadline(
    sender: &Sender,
    owner: Uuid,
    body: envelope::Body,
    expires_at: Option<u64>,
) -> Result<(), ErrorCode> {
    sender
        .try_send(SessionMessage::Event(CloudEvent::Ysm {
            owner,
            body,
            expires_at,
        }))
        .map_err(|_| ErrorCode::SnapshotRequired)
}

async fn recover(
    state: &AppState,
    sender: &Sender,
    owner: Uuid,
    viewer: Uuid,
    cursor: Option<&Resume>,
) -> Result<Option<YsmIdentity>, ErrorCode> {
    if !crate::service::library::can_observe_model(state, owner, viewer).await? {
        return Ok(None);
    }
    let now = crate::storage::now_seconds();
    if let Some(cursor) = cursor {
        match state
            .storage
            .ysm_resume(cursor, state.protocol_sessions.server_epoch, now)
        {
            Ok(frames) => {
                let mut baseline = YsmIdentity {
                    owner: cursor.owner.clone(),
                    asset_revision: cursor.revision,
                    publisher_session: cursor.publisher_session.clone(),
                    generation: cursor.generation,
                    sequence: cursor.sequence,
                    event_watermark: cursor.event_watermark,
                    ..Default::default()
                };
                for body in frames {
                    baseline = ysm::identity(&body)?.clone();
                    let deadline = if matches!(&body, envelope::Body::YsmEvent(_)) {
                        Some(state.storage.ysm_event_deadline(owner, &baseline)?)
                    } else {
                        None
                    };
                    enqueue_with_deadline(sender, owner, body, deadline)?;
                }
                return Ok(Some(baseline));
            }
            Err(code) => enqueue(sender, owner, error_body(code, 0))?,
        }
    }
    let equipped = state.storage.equipped(owner)?;
    if !equipped
        .primary
        .as_ref()
        .is_some_and(|avatar| matches!(avatar.model_type, 2 | 3))
    {
        return Ok(None);
    }
    match state.storage.ysm_snapshot(owner, now) {
        Ok(snapshot) => {
            let baseline = snapshot.identity.clone();
            enqueue(sender, owner, envelope::Body::YsmSnapshot(snapshot))?;
            Ok(baseline)
        }
        Err(code) => {
            enqueue(sender, owner, error_body(code, 0))?;
            crate::service::event::request_snapshot(state, owner, code);
            Ok(None)
        }
    }
}

pub fn spawn(
    state: AppState,
    owner: Uuid,
    viewer: Uuid,
    cursor: Option<Resume>,
    sender: Sender,
    supports_ysm: bool,
) -> AbortHandle {
    // Subscribe before reading recovery state. Frames already included in that
    // state remain queued in broadcast and are discarded by their identity/sequence.
    let mut events = state.subscribes.subscribe(owner);
    tokio::spawn(async move {
        let result: Result<(), ErrorCode> = async {
            let mut baseline = if supports_ysm {
                recover(&state, &sender, owner, viewer, cursor.as_ref()).await?
            } else {
                None
            };
            loop {
                let event = events
                    .recv()
                    .await
                    .map_err(|_| ErrorCode::SnapshotRequired)?;
                if !crate::service::library::event_visible(&state, viewer, &event).await? {
                    baseline = None;
                    continue;
                }
                match &event {
                    CloudEvent::Ysm { body, .. } => {
                        if !supports_ysm || !ysm::advance_cursor(&mut baseline, body)? {
                            continue;
                        }
                    }
                    CloudEvent::AvatarChanged { .. } => {
                        baseline = None;
                        sender
                            .try_send(SessionMessage::Event(event.clone()))
                            .map_err(|_| ErrorCode::SnapshotRequired)?;
                        if supports_ysm {
                            baseline = recover(&state, &sender, owner, viewer, None).await?;
                        }
                        continue;
                    }
                    _ => {}
                }
                sender
                    .try_send(SessionMessage::Event(event))
                    .map_err(|_| ErrorCode::SnapshotRequired)?;
            }
        }
        .await;
        if result.is_err() {
            sender.close_signal().send_replace(true);
        }
    })
    .abort_handle()
}
