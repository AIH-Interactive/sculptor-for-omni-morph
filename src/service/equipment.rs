use super::event::CloudEvent;
use crate::{api::figura::SessionMessage, AppState};
use uuid::Uuid;

pub async fn changed(state: &AppState, owner: Uuid) {
    notify(state, owner, false).await;
}

pub async fn deleted(state: &AppState, owner: Uuid) {
    notify(state, owner, true).await;
}

async fn notify(state: &AppState, owner: Uuid, tombstone: bool) {
    let event = CloudEvent::AvatarChanged { owner, tombstone };
    let _ = state.subscribes.publisher(owner).send(event.clone());
    if let Some(sessions) = state.session.get(&owner) {
        let _ = sessions.send(SessionMessage::Event(event)).await;
    }
}
