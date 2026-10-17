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
    /// Privileged legacy administration endpoint; never forwarded to Protobuf.
    LegacyRaw(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct EventHub {
    channels: DashMap<Uuid, broadcast::Sender<CloudEvent>>,
}

impl EventHub {
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
