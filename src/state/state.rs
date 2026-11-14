use std::sync::Arc;

use tokio::{sync::RwLock, time::Instant};

use crate::{
    auth::UManager,
    service::{event::EventHub, session::{SessionRegistry, ProtocolSessions}},
    FiguraVersions,
};

#[derive(Debug, Clone)]
pub struct AppState {
    /// Uptime
    pub uptime: Instant,
    /// User manager
    pub user_manager: Arc<UManager>,
    /// Send into WebSocket
    pub session: Arc<SessionRegistry>,
    /// Send messages for subscribers
    pub subscribes: Arc<EventHub>,
    pub protocol_sessions: Arc<ProtocolSessions>,
    pub storage: Arc<crate::storage::Storage>,
    pub transfers: Arc<crate::service::resource::Transfers>,
    /// Current configuration
    pub config: Arc<RwLock<super::Config>>,
    /// Caching Figura Versions
    pub figura_versions: Arc<RwLock<Option<FiguraVersions>>>,
}
