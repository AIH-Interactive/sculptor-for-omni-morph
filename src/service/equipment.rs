use super::event::CloudEvent;
use crate::{api::figura::SessionMessage, AppState};
use uuid::Uuid;

pub async fn changed(state: &AppState, owner: Uuid) {
    let event = CloudEvent::AvatarChanged { owner };
    let _ = state.subscribes.publisher(owner).send(event.clone());
    if let Some(sessions) = state.session.get(&owner) {
        let _ = sessions.send(SessionMessage::Event(event)).await;
    }
}
