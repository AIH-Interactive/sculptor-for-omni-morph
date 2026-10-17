use crate::protocol::v1::{Auth, AuthResult, ErrorCode};
use crate::{api::figura::SessionMessage, auth::Userinfo, ApiError, ApiResult, AppState};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct ScopedSession {
    pub id: String,
    pub owner: Uuid,
    pub epoch: u64,
    source_token: String,
    deadline: Instant,
    expires_unix_seconds: u64,
    connections: usize,
}

struct BudgetWindow {
    started: Instant,
    messages: u32,
    bytes: usize,
}

pub struct ProtocolSessions {
    pub instance_id: String,
    pub server_epoch: u64,
    sessions: DashMap<String, ScopedSession>,
    budgets: Mutex<HashMap<Uuid, BudgetWindow>>,
    issue_lock: Mutex<()>,
}

impl std::fmt::Debug for ProtocolSessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProtocolSessions")
            .finish_non_exhaustive()
    }
}

impl ProtocolSessions {
    pub fn new(instance_id: String) -> Self {
        let random = crate::utils::rand();
        Self {
            instance_id,
            server_epoch: u64::from_be_bytes(random[..8].try_into().expect("eight-byte epoch")),
            sessions: DashMap::new(),
            budgets: Mutex::new(HashMap::new()),
            issue_lock: Mutex::new(()),
        }
    }

    pub fn issue(
        &self,
        state: &AppState,
        auth: &Auth,
        epoch: u64,
    ) -> Result<AuthResult, ErrorCode> {
        if auth.instance_id != self.instance_id || epoch == 0 {
            return Err(ErrorCode::Unauthenticated);
        }
        let identity = authenticate(state, &auth.token).map_err(|_| ErrorCode::Unauthenticated)?;
        let _issue = self.issue_lock.lock().map_err(|_| ErrorCode::Internal)?;
        self.sessions
            .retain(|_, session| session.deadline > Instant::now());
        if let Some(existing) = self
            .sessions
            .iter()
            .find(|session| session.source_token == auth.token && session.epoch == epoch)
        {
            return Ok(AuthResult {
                owner: existing.owner.to_string(),
                session_id: existing.id.clone(),
                expires_unix_seconds: existing.expires_unix_seconds,
                instance_id: self.instance_id.clone(),
                protocol_major: 1,
            });
        }
        if self
            .sessions
            .iter()
            .filter(|entry| entry.owner == identity.uuid)
            .count()
            >= 8
        {
            return Err(ErrorCode::LimitExceeded);
        }
        let session_id = faster_hex::hex_string(&crate::utils::rand()[..32]);
        self.sessions.insert(
            session_id.clone(),
            ScopedSession {
                id: session_id.clone(),
                owner: identity.uuid,
                epoch,
                source_token: auth.token.clone(),
                deadline: Instant::now() + Duration::from_secs(3600),
                expires_unix_seconds: crate::storage::now_seconds() + 3600,
                connections: 0,
            },
        );
        Ok(AuthResult {
            owner: identity.uuid.to_string(),
            session_id,
            expires_unix_seconds: crate::storage::now_seconds() + 3600,
            instance_id: self.instance_id.clone(),
            protocol_major: 1,
        })
    }

    pub fn authorize(
        &self,
        state: &AppState,
        id: &str,
        epoch: u64,
    ) -> Result<ScopedSession, ErrorCode> {
        let session = self
            .sessions
            .get(id)
            .map(|session| session.clone())
            .ok_or(ErrorCode::Unauthenticated)?;
        if session.deadline <= Instant::now() {
            self.sessions.remove(id);
            return Err(ErrorCode::Unauthenticated);
        }
        let identity =
            authenticate(state, &session.source_token).map_err(|_| ErrorCode::Unauthenticated)?;
        if identity.uuid != session.owner || session.epoch != epoch {
            return Err(ErrorCode::Unauthenticated);
        }
        Ok(session)
    }

    pub fn attach(&self, id: &str) -> Result<(), ErrorCode> {
        let mut session = self
            .sessions
            .get_mut(id)
            .ok_or(ErrorCode::Unauthenticated)?;
        if session.deadline <= Instant::now() {
            return Err(ErrorCode::Unauthenticated);
        }
        session.connections += 1;
        Ok(())
    }

    pub fn detach(&self, id: &str) {
        if let dashmap::mapref::entry::Entry::Occupied(mut entry) =
            self.sessions.entry(id.to_owned())
        {
            entry.get_mut().connections = entry.get().connections.saturating_sub(1);
            if entry.get().connections == 0 {
                entry.remove();
            }
        }
    }

    /// Both adapters charge this same account window; switching protocol cannot reset it.
    pub fn charge(&self, owner: Uuid, bytes: usize) -> Result<(), ErrorCode> {
        let mut budgets = self.budgets.lock().map_err(|_| ErrorCode::Internal)?;
        let budget = budgets.entry(owner).or_insert_with(|| BudgetWindow {
            started: Instant::now(),
            messages: 0,
            bytes: 0,
        });
        if budget.started.elapsed() >= Duration::from_secs(1) {
            *budget = BudgetWindow {
                started: Instant::now(),
                messages: 0,
                bytes: 0,
            };
        }
        if budget.messages >= 32 || bytes > 262_144usize.saturating_sub(budget.bytes) {
            return Err(ErrorCode::LimitExceeded);
        }
        budget.messages += 1;
        budget.bytes += bytes;
        Ok(())
    }
}

#[derive(Debug)]
struct Connection {
    owner: Uuid,
    sender: super::queue::Sender,
    close: watch::Sender<bool>,
}

/// A transport owns exactly one ID. Disconnect never revokes another transport.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    next_id: AtomicU64,
    connections: DashMap<u64, Connection>,
    admission: Mutex<()>,
}

impl SessionRegistry {
    fn register(&self, owner: Uuid, sender: super::queue::Sender) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let close = sender.close_signal();
        self.connections.insert(
            id,
            Connection {
                owner,
                sender,
                close,
            },
        );
        id
    }

    pub fn register_bounded(
        &self,
        owner: Uuid,
        sender: super::queue::Sender,
    ) -> Result<u64, ErrorCode> {
        let _admission = self.admission.lock().map_err(|_| ErrorCode::Internal)?;
        if self.count(owner) >= 8 {
            return Err(ErrorCode::LimitExceeded);
        }
        Ok(self.register(owner, sender))
    }

    pub fn close_signal(&self, id: u64) -> Option<(watch::Sender<bool>, watch::Receiver<bool>)> {
        self.connections
            .get(&id)
            .map(|connection| (connection.close.clone(), connection.close.subscribe()))
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
            .map(|entry| (entry.sender.clone(), entry.close.clone()))
            .collect();
        if senders.is_empty() {
            None
        } else {
            Some(Recipients(senders))
        }
    }
}

pub struct Recipients(Vec<(super::queue::Sender, watch::Sender<bool>)>);

impl Recipients {
    pub async fn send(
        &self,
        message: SessionMessage,
    ) -> Result<(), mpsc::error::SendError<SessionMessage>> {
        self.try_send(message)
    }

    pub fn try_send(
        &self,
        message: SessionMessage,
    ) -> Result<(), mpsc::error::SendError<SessionMessage>> {
        let mut delivered = false;
        // Snapshotting recipients above ensures no DashMap lock is held across await.
        for (sender, close) in &self.0 {
            match sender.try_send(message.clone()) {
                Ok(()) => delivered = true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    close.send_replace(true);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
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
    async fn account_limits_and_slow_recipient_are_bounded() {
        let owner = Uuid::from_u128(700);
        let sessions = ProtocolSessions::new("test-instance".into());
        for _ in 0..32 {
            sessions.charge(owner, 1).unwrap();
        }
        assert_eq!(sessions.charge(owner, 1), Err(ErrorCode::LimitExceeded));
        let other = Uuid::from_u128(701);
        sessions.charge(other, 262_144).unwrap();
        assert_eq!(sessions.charge(other, 1), Err(ErrorCode::LimitExceeded));

        let registry = SessionRegistry::default();
        let mut receivers = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..8 {
            let (sender, receiver) = crate::service::queue::channel(1);
            receivers.push(receiver);
            ids.push(registry.register_bounded(owner, sender).unwrap());
        }
        let (sender, _receiver) = crate::service::queue::channel(1);
        assert_eq!(
            registry.register_bounded(owner, sender),
            Err(ErrorCode::LimitExceeded)
        );
        let (_, closed) = registry.close_signal(ids[0]).unwrap();
        registry
            .get(&owner)
            .unwrap()
            .send(SessionMessage::Banned)
            .await
            .unwrap();
        assert!(registry
            .get(&owner)
            .unwrap()
            .send(SessionMessage::Banned)
            .await
            .is_err());
        assert!(*closed.borrow());
        registry.remove(ids[0]);
        let (sender, _receiver) = crate::service::queue::channel(1);
        registry.register_bounded(owner, sender).unwrap();
    }

    #[tokio::test]
    async fn disconnect_removes_only_its_connection() {
        let registry = SessionRegistry::default();
        let (first, mut first_rx) = crate::service::queue::channel(4);
        let (second, mut second_rx) = crate::service::queue::channel(4);
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
