use std::sync::Arc;

use anyhow::{anyhow, Context};
use axum::{
    async_trait, extract::{FromRequestParts, State}, http::{request::Parts, StatusCode}
};
use dashmap::DashMap;
use thiserror::Error;
use tracing::{debug, error, trace};
use uuid::Uuid;

use crate::{ApiError, ApiResult, AppState, TIMEOUT, USER_AGENT};
use super::types::*;

// It's an extractor that pulls a token from the Header.
#[derive(PartialEq, Debug)]
pub struct Token(pub String);

impl Token {
    pub async fn check_auth(self, state: &AppState) -> ApiResult<()> {
        crate::service::session::authenticate(state, &self.0).map(|_| ())
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for Token
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("token")
            .and_then(|value| value.to_str().ok());
        trace!(present = token.is_some(), "Authentication header inspected");
        match token {
            Some(token) => Ok(Self(token.to_string())),
            None => Err(StatusCode::UNAUTHORIZED),
        }
    }
}
// End Extractor

// Work with external APIs
/// Get UUID from JSON response
fn get_id_json(json: &serde_json::Value) -> Result<Uuid, uuid::Error> {
    let uuid = Uuid::parse_str(json.get("id").unwrap().as_str().unwrap())?;
    Ok(uuid)
}

#[derive(Debug, Error)]
enum FetchError {
    #[error("authentication provider returned HTTP {0}")]
    WrongResponse(u16),
    #[error(transparent)]
    SendError(#[from] reqwest::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),

}

async fn fetch_json(
    State(state): State<AppState>,
    auth_provider: &AuthProvider,
    server_id: &str,
    username: &str,
    ip: String,
) -> Result<(Uuid, AuthProvider), FetchError> {
    let client = reqwest::Client::builder().timeout(TIMEOUT).user_agent(USER_AGENT).build().unwrap();
    let url = auth_provider.url.clone();

    let mut query = vec![
        ("serverId", server_id.to_string()),
        ("username", username.to_string()),
    ];

    if auth_provider.name == "Offline" {
        query.push(("ip", ip));
    }

    let res = client
        .get(url)
        .query(&query)
        .send()
        .await?;
    trace!(status = res.status().as_u16(), "Authentication provider responded");
    match res.status().as_u16() {
        200 => {
            let json = serde_json::from_str::<serde_json::Value>(&res.text().await?).with_context(|| "Cant deserialize".to_string())?;
            let uuid = get_id_json(&json).with_context(|| "Cant get UUID".to_string())?;
            let def_can_upload = state.config.read().await.limitations.can_upload;
            let can_upload = json.get("can_upload")
                .map(|s| s.as_bool().unwrap_or(def_can_upload))
                .unwrap_or(def_can_upload);
            state
                .user_manager
                .put_upload_state(uuid, can_upload)
                .map_err(|code| {
                    anyhow::anyhow!("Account upload permission commit failed: {code:?}")
                })?;
            Ok((uuid, auth_provider.clone()))
        }
        _ => Err(FetchError::WrongResponse(res.status().as_u16())),
    }
}

pub async fn has_joined(
    State(state): State<AppState>,
    server_id: &str,
    username: &str,
    ip: String,
) -> anyhow::Result<Option<(Uuid, AuthProvider)>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let AuthProviders(auth_providers) = state.config.read().await.auth_providers.clone();

    for provider in &auth_providers {
        tokio::spawn(fetch_and_send(
            State(state.clone()),
            provider.clone(),
            server_id.to_string(),
            username.to_string(),
            ip.clone(),
            tx.clone()
        ));
    } 
    let mut errors = Vec::new(); // Counting fetches what returns errors
    let mut misses = Vec::new(); // Counting non OK results
    let mut prov_count: usize = auth_providers.len();
    while prov_count > 0 {
        if let Some(fetch_res) = rx.recv().await {
            match fetch_res {
                Ok(data) => return Ok(Some(data)),
                Err(err) => {
                    match err {
                        FetchError::WrongResponse(code) => misses.push(code),
                        FetchError::SendError(_) => errors.push("provider_transport"),
                        FetchError::Other(_) => errors.push("provider_response"),
                    }
                },
            }
        } else {
            error!("Unexpected behavior!");
            return Err(anyhow!("Something went wrong..."))
        }
        prov_count -= 1;
    }

    // Choosing what error return

    // Returns if some internals errors occured
    if !errors.is_empty() {
        error!(misses = misses.len(), errors = errors.len(), "Authentication providers failed");
        Err(anyhow::anyhow!("Authentication providers failed"))
        
    } else {
        // Returning if user can't be authenticated
        debug!(misses = misses.len(), "Authentication providers did not authenticate the user");
        Ok(None)
    }
}

async fn fetch_and_send(
    State(state): State<AppState>,
    provider: AuthProvider,
    server_id: String,
    username: String,
    ip: String,
    tx: tokio::sync::mpsc::Sender<Result<(Uuid, AuthProvider), FetchError>>
) {
    let _ = tx.send(fetch_json(State(state), &provider, &server_id, &username, ip).await)
        .await.map_err(|_| trace!("Authentication result receiver closed"));
}

// User manager
#[derive(Debug, Clone)]
pub struct UManager {
    storage: Option<Arc<crate::storage::Storage>>,
    writes: Arc<std::sync::Mutex<()>>,
    /// Users with incomplete authentication
    pending: Arc<DashMap<String, String>>, // <SHA1 serverId, USERNAME> TODO: Add automatic purge
    /// Authenticated users TODO: Change name to sessions
    authenticated: Arc<DashMap<String, Uuid>>, // <SHA1 serverId, Userinfo>
    /// Registered users
    registered: Arc<DashMap<Uuid, Userinfo>>,
    /// uploadState
    can_upload: Arc<DashMap<Uuid, bool>>,
    /// temp state
    requested_temp: Arc<DashMap<Uuid, bool>>,
}

impl UManager {
    pub fn open(storage: Arc<crate::storage::Storage>) -> crate::storage::Result<Self> {
        let mut manager = Self::new();
        for user in storage.users()? {
            manager.registered.insert(user.uuid, user);
        }
        for (uuid, allowed) in storage.upload_permissions()? {
            manager.can_upload.insert(uuid, allowed);
        }
        manager.storage = Some(storage);
        Ok(manager)
    }

    pub fn new() -> Self {
        Self {
            storage: None,
            writes: Arc::new(std::sync::Mutex::new(())),
            pending: Arc::new(DashMap::new()),
            registered: Arc::new(DashMap::new()),
            authenticated: Arc::new(DashMap::new()),
            can_upload: Arc::new(DashMap::new()),
            requested_temp: Arc::new(DashMap::new()),
        }
    }
    pub fn get_all_registered(&self) -> DashMap<Uuid, Userinfo> {
        self.registered.as_ref().clone()
    }
    pub fn get_all_authenticated(&self) -> DashMap<String, Uuid> {
        self.authenticated.as_ref().clone()
    }
    pub fn pending_insert(&self, server_id: String, username: String) {
        self.pending.insert(server_id, username);
    }
    pub fn pending_remove(&self, server_id: &str) -> Option<(String, String)> {
        self.pending.remove(server_id)
    }
    pub fn insert(&self, uuid: Uuid, token: String, userinfo: Userinfo) -> Result<(), ()> {
        // Multiple authenticated device tokens may refer to one account.
        self.insert_user(uuid, userinfo).map_err(|_| ())?;
        self.authenticated.insert(token, uuid);
        Ok(())
    }
    pub fn insert_user(&self, uuid: Uuid, userinfo: Userinfo) -> crate::storage::Result<()> {
        if userinfo.uuid != uuid {
            return Err(crate::protocol::v1::ErrorCode::InvalidMessage);
        }
        self.update_user(uuid, |existing| {
            if let Some(existing) = existing {
                let defaults = Userinfo::default();
                if !userinfo.nickname.is_empty() {
                    existing.nickname = userinfo.nickname;
                }
                if !userinfo.auth_provider.is_empty() {
                    existing.auth_provider = userinfo.auth_provider;
                }
                if userinfo.rank != defaults.rank {
                    existing.rank = userinfo.rank;
                }
                if userinfo.token.is_some() {
                    existing.token = userinfo.token;
                }
                if userinfo.version != defaults.version {
                    existing.version = userinfo.version;
                }
                existing.last_used = userinfo.last_used;
            } else {
                *existing = Some(userinfo);
            }
        })
    }

    fn update_user(
        &self,
        uuid: Uuid,
        update: impl FnOnce(&mut Option<Userinfo>),
    ) -> crate::storage::Result<()> {
        let _write = self
            .writes
            .lock()
            .map_err(|_| crate::protocol::v1::ErrorCode::Internal)?;
        let mut candidate = self.registered.get(&uuid).map(|user| user.clone());
        update(&mut candidate);
        if let Some(user) = candidate {
            if let Some(storage) = &self.storage {
                storage.save_user(&user)?;
            }
            self.registered.insert(uuid, user);
        }
        Ok(())
    }
    pub fn get(&self, token: &String) -> Option<dashmap::mapref::one::Ref<'_, Uuid, Userinfo>> {
        let uuid = self.authenticated.get(token)?;
        self.registered.get(uuid.value())
    }
    pub fn get_by_uuid(
        &self,
        uuid: &Uuid,
    ) -> Option<dashmap::mapref::one::Ref<'_, Uuid, Userinfo>> {
        self.registered.get(uuid)
    }

    pub fn restore_model_owner(&self, uuid: Uuid) -> crate::storage::Result<()> {
        if !self.registered.contains_key(&uuid)
            && self
                .storage
                .as_ref()
                .is_some_and(|storage| storage.has_owner(uuid))
        {
            self.insert_user(
                uuid,
                Userinfo {
                    uuid,
                    ..Default::default()
                },
            )?;
        }
        Ok(())
    }
    pub fn ban(&self, banned_user: &Userinfo) -> crate::storage::Result<()> {
        self.update_user(banned_user.uuid, |user| {
            user.get_or_insert_with(|| banned_user.clone()).banned = true;
        })
    }
    pub fn unban(&self, uuid: &Uuid) -> crate::storage::Result<()> {
        self.update_user(*uuid, |user| {
            if let Some(user) = user {
                user.banned = false;
            }
        })
    }
    pub fn _is_authenticated(&self, token: &String) -> bool {
        self.authenticated.contains_key(token)
    }
    pub fn _is_registered(&self, uuid: &Uuid) -> bool {
        self.registered.contains_key(uuid)
    }
    pub fn is_banned(&self, uuid: &Uuid) -> bool {
        if let Some(user) = self.registered.get(uuid) { user.banned } else { false }
    }
    pub fn count_authenticated(&self) -> usize {
        self.authenticated.len()
    }
    pub fn put_upload_state(&self, uuid: Uuid, upload_state: bool) -> crate::storage::Result<()> {
        let _write = self
            .writes
            .lock()
            .map_err(|_| crate::protocol::v1::ErrorCode::Internal)?;
        if let Some(storage) = &self.storage {
            storage.save_upload_permission(uuid, upload_state)?;
        }
        self.can_upload.insert(uuid, upload_state);
        Ok(())
    }
    pub fn upload_state(&self, uuid: Uuid, def: bool) -> bool {
        self.can_upload.get(&uuid)
            .map(|upload_state| { *upload_state.value() })
            .unwrap_or(def)
    }
    pub fn put_request_temp_state(&self, uuid: Uuid, temp_state: bool) {
        self.requested_temp.insert(uuid, temp_state);
    }
    pub fn request_temp_state(&self, uuid: Uuid, def: bool) -> bool {
        self.requested_temp.get(&uuid)
            .map(|temp_state| { *temp_state.value() })
            .unwrap_or(def)
    }
}
// End of User manager

pub async fn check_auth(
    token: Option<Token>,
    State(state): State<AppState>,
) -> ApiResult<&'static str> {
    debug!("Checking auth actuality...");
    match token {
        Some(token) => {
            token.check_auth(&state).await?;
            Ok("ok")
        },
        None => Err(ApiError::BadRequest), 
    }
}
