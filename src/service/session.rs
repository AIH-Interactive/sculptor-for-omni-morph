use crate::{api::figura::SessionMessage, auth::Userinfo, ApiError, ApiResult, AppState};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug)]
struct Connection {
    owner: Uuid,
    sender: mpsc::Sender<SessionMessage>,
}

/// A transport owns exactly one ID. Disconnect never revokes another transport.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    next_id: AtomicU64,
    connections: DashMap<u64, Connection>,
}

impl SessionRegistry {
    pub fn register(&self, owner: Uuid, sender: mpsc::Sender<SessionMessage>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.connections.insert(id, Connection { owner, sender });
        id
    }

    pub fn remove(&self, id: u64) {
        self.connections.remove(&id);
    }

    pub fn count(&self, owner: Uuid) -> usize {
        self.connections
            .iter()
            .filter(|entry| entry.owner == owner)
            .count()
    }

    pub fn get(&self, owner: &Uuid) -> Option<Recipients> {
        let senders: Vec<_> = self
            .connections
            .iter()
            .filter(|entry| entry.owner == *owner)
            .map(|entry| entry.sender.clone())
            .collect();
        if senders.is_empty() {
            None
        } else {
            Some(Recipients(senders))
        }
    }
}

pub struct Recipients(Vec<mpsc::Sender<SessionMessage>>);

impl Recipients {
    pub async fn send(
        &self,
        message: SessionMessage,
    ) -> Result<(), mpsc::error::SendError<SessionMessage>> {
        let mut delivered = false;
        // Snapshotting recipients above ensures no DashMap lock is held across await.
        for sender in &self.0 {
            if sender.send(message.clone()).await.is_ok() {
                delivered = true;
            }
        }
        if delivered {
            Ok(())
        } else {
            Err(mpsc::error::SendError(message))
        }
    }
}

pub fn authenticate(state: &AppState, token: &str) -> ApiResult<Userinfo> {
    state
        .user_manager
        .get(&token.to_owned())
        .filter(|user| !user.banned)
        .map(|user| user.clone())
        .ok_or(ApiError::Unauthorized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disconnect_removes_only_its_connection() {
        let registry = SessionRegistry::default();
        let (first, mut first_rx) = mpsc::channel(4);
        let (second, mut second_rx) = mpsc::channel(4);
        let owner = Uuid::nil();
        let first_id = registry.register(owner, first);
        let second_id = registry.register(owner, second);
        registry
            .get(&owner)
            .unwrap()
            .send(SessionMessage::Banned)
            .await
            .unwrap();
        assert!(matches!(
            first_rx.recv().await,
            Some(SessionMessage::Banned)
        ));
        assert!(matches!(
            second_rx.recv().await,
            Some(SessionMessage::Banned)
        ));
        registry.remove(first_id);
        registry.remove(first_id);
        registry
            .get(&owner)
            .unwrap()
            .send(SessionMessage::Ping(vec![7]))
            .await
            .unwrap();
        assert!(
            matches!(second_rx.recv().await, Some(SessionMessage::Ping(bytes)) if bytes == [7])
        );
        assert!(first_rx.recv().await.is_none());
        registry.remove(second_id);
        assert!(registry.get(&owner).is_none());
    }
}
