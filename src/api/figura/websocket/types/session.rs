use dashmap::DashMap;
use tokio::{sync::broadcast, task::AbortHandle};

pub struct WSSession {
    pub user: crate::auth::Userinfo,
    pub own_tx: crate::service::queue::Sender,
    pub own_rx: crate::service::queue::Receiver,
    pub subs_tx: broadcast::Sender<crate::service::event::CloudEvent>,
    pub sub_workers_aborthandles: DashMap<uuid::Uuid, AbortHandle>,
}

#[derive(Clone, Debug)]
pub enum SessionMessage {
    Event(crate::service::event::CloudEvent),
    Ping(Vec<u8>),
    Banned,
    ResyncRequired,
}
