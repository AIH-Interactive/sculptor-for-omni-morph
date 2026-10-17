use dashmap::DashMap;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Business events are encoded separately by each connection's protocol adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum CloudEvent {
    FiguraPing {
        owner: Uuid,
        function_id: u32,
        echo: bool,
        arguments: Vec<u8>,
    },
    AvatarChanged {
        owner: Uuid,
        tombstone: bool,
    },
    Ysm {
        owner: Uuid,
        body: crate::protocol::v1::envelope::Body,
        expires_at: Option<u64>,
    },
    /// Privileged legacy administration endpoint; never forwarded to Protobuf.
    LegacyRaw(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct EventHub {
    channels: DashMap<Uuid, broadcast::Sender<CloudEvent>>,
    snapshot_requests: std::sync::Mutex<std::collections::HashMap<Uuid, std::time::Instant>>,
}

impl EventHub {
    fn request_due(&self, owner: Uuid) -> bool {
        let Ok(mut requests) = self.snapshot_requests.lock() else {
            return false;
        };
        requests.retain(|_, time| time.elapsed() < std::time::Duration::from_secs(1));
        if requests.contains_key(&owner) {
            return false;
        }
        requests.insert(owner, std::time::Instant::now());
        true
    }
    pub fn publisher(&self, owner: Uuid) -> broadcast::Sender<CloudEvent> {
        self.channels
            .entry(owner)
            .or_insert_with(|| broadcast::channel(32).0)
            .clone()
    }

    pub fn get(&self, owner: &Uuid) -> Option<broadcast::Sender<CloudEvent>> {
        self.channels.get(owner).map(|entry| entry.clone())
    }

    pub fn subscribe(&self, owner: Uuid) -> broadcast::Receiver<CloudEvent> {
        self.publisher(owner).subscribe()
    }
}

pub fn request_snapshot(
    state: &crate::AppState,
    owner: Uuid,
    reason: crate::protocol::v1::ErrorCode,
) {
    let Some(sessions) = state.session.get(&owner) else {
        return;
    };
    if !state.subscribes.request_due(owner) {
        return;
    }
    let _ = sessions.try_send(crate::api::figura::SessionMessage::Event(CloudEvent::Ysm {
        owner,
        body: crate::protocol::v1::envelope::Body::SnapshotRequest(
            crate::protocol::v1::SnapshotRequest {
                owner: owner.to_string(),
                reason: reason as i32,
            },
        ),
        expires_at: None,
    }));
}
