//! Configuration-backed administrator credentials. No mutable account database.
use crate::storage::library::sql_integer;
use crate::{protocol::v1::ErrorCode, storage::Storage};
use ring::pbkdf2;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use std::{
    collections::HashSet,
    num::NonZeroU32,
    sync::{Arc, LazyLock},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

pub mod operations;

type Result<T> = std::result::Result<T, ErrorCode>;
static KDF_WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
const ACCESS_SECONDS: u64 = 15 * 60;
const REFRESH_SECONDS: u64 = 7 * 86400;
pub const BUILTIN_CATEGORIES: [&str; 2] = ["full_model", "decorative_parts"];

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawLibraryConfig")]
pub struct LibraryConfig {
    pub admins: Vec<AdminConfig>,
    pub max_upload_bytes: u64,
    pub preview_cache_seconds: u32,
    pub categories: Vec<String>,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            admins: Vec::new(),
            max_upload_bytes: 64 * 1024 * 1024,
            preview_cache_seconds: 3600,
            categories: Vec::new(),
        }
    }
}

impl LibraryConfig {
    pub fn effective_categories(&self) -> Vec<String> {
        BUILTIN_CATEGORIES.iter().map(|id| (*id).to_owned())
            .chain(self.categories.iter().cloned()).collect()
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLibraryConfig {
    #[serde(default)]
    admins: Vec<AdminConfig>,
    #[serde(default)]
    max_upload_bytes: Option<u64>,
    #[serde(default)]
    preview_cache_seconds: Option<u32>,
    #[serde(default)]
    categories: Option<Vec<String>>,
}

impl TryFrom<RawLibraryConfig> for LibraryConfig {
    type Error = &'static str;

    fn try_from(raw: RawLibraryConfig) -> std::result::Result<Self, Self::Error> {
        let mut uuids = HashSet::new();
        let mut names = HashSet::new();
        for admin in &raw.admins {
            if admin.uuid.is_nil()
                || admin.player_name.is_empty()
                || admin.player_name.chars().count() > 16
                || !admin
                    .player_name
                    .chars()
                    .all(|character| {
                        character.is_ascii_alphanumeric()
                            || character == '_'
                            || ('\u{0080}'..='\u{d7ff}').contains(&character)
                    })
            {
                return Err("invalid model library administrator identity");
            }
            if !uuids.insert(admin.uuid) || !names.insert(admin.player_name.to_ascii_lowercase()) {
                return Err("duplicate model library administrator identity");
            }
            PasswordHash::parse(&admin.password_hash)
                .map_err(|_| "invalid model library administrator password hash")?;
        }
        let max_upload_bytes = raw.max_upload_bytes.unwrap_or(64 * 1024 * 1024);
        let categories = raw.categories.unwrap_or_default();
        let mut unique_categories = HashSet::new();
        if max_upload_bytes == 0
            || max_upload_bytes > 1024 * 1024 * 1024
            || categories.len() > 30
            || categories.iter().any(|category| {
                category.is_empty()
                    || BUILTIN_CATEGORIES.contains(&category.as_str())
                    || category.len() > 32
                    || !category.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-')
                    })
                    || !unique_categories.insert(category)
            })
        {
            return Err("invalid model library limits or categories");
        }
        Ok(Self {
            admins: raw.admins,
            max_upload_bytes,
            preview_cache_seconds: raw.preview_cache_seconds.unwrap_or(3600),
            categories,
        })
    }
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub player_name: String,
    pub uuid: Uuid,
    password_hash: String,
}

impl std::fmt::Debug for AdminConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdminConfig")
            .field("uuid", &self.uuid)
            .field("player_name", &self.player_name)
            .finish_non_exhaustive()
    }
}

struct PasswordHash {
    iterations: NonZeroU32,
    salt: [u8; 16],
    key: [u8; 32],
}

impl PasswordHash {
    fn parse(encoded: &str) -> Result<Self> {
        if encoded.len() > 160 {
            return Err(ErrorCode::InvalidMessage);
        }
        let parts: Vec<_> = encoded.split('$').collect();
        if parts.len() != 5 || parts[0] != "pbkdf2-sha256" || parts[1] != "v1" {
            return Err(ErrorCode::InvalidMessage);
        }
        let iterations: u32 = parts[2].parse().map_err(|_| ErrorCode::InvalidMessage)?;
        if !(600_000..=2_000_000).contains(&iterations) || parts[2] != iterations.to_string() {
            return Err(ErrorCode::InvalidMessage);
        }
        fn decode<const N: usize>(text: &str) -> Result<[u8; N]> {
            if text.len() != N * 2
                || !text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(ErrorCode::InvalidMessage);
            }
            let mut bytes = [0; N];
            faster_hex::hex_decode(text.as_bytes(), &mut bytes)
                .map_err(|_| ErrorCode::InvalidMessage)?;
            Ok(bytes)
        }
        Ok(Self {
            iterations: NonZeroU32::new(iterations).ok_or(ErrorCode::InvalidMessage)?,
            salt: decode(parts[3])?,
            key: decode(parts[4])?,
        })
    }
}

impl AdminConfig {
    pub fn fingerprint(&self) -> Vec<u8> {
        let mut bytes = self.uuid.as_bytes().to_vec();
        bytes.extend_from_slice(self.player_name.to_ascii_lowercase().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.password_hash.as_bytes());
        crate::storage::sha256(&bytes)
    }

    pub async fn verify_password(&self, player: Uuid, name: &str, password: String) -> Result<()> {
        let mut password = password.into_bytes();
        if player != self.uuid
            || !name.eq_ignore_ascii_case(&self.player_name)
            || password.is_empty()
            || password.len() > 1024
        {
            password.fill(0);
            return Err(ErrorCode::Forbidden);
        }
        let parsed = PasswordHash::parse(&self.password_hash)?;
        // Reject excess work instead of creating an unbounded blocking queue.
        let permit = match KDF_WORKERS.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                password.fill(0);
                return Err(ErrorCode::LimitExceeded);
            }
        };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let verified = pbkdf2::verify(
                pbkdf2::PBKDF2_HMAC_SHA256,
                parsed.iterations,
                &parsed.salt,
                &password,
                &parsed.key,
            )
            .map_err(|_| ErrorCode::Forbidden);
            password.fill(0);
            verified
        })
        .await
        .map_err(|_| ErrorCode::Internal)?
    }
}

/// Credentials are returned exactly once; deliberately no Debug implementation.
pub struct AdminCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}

pub struct AdminBinding<'a> {
    pub player: Uuid,
    pub origin: &'a str,
    pub instance: &'a str,
    pub config: &'a AdminConfig,
}

impl AdminBinding<'_> {
    fn valid(&self) -> bool {
        self.player == self.config.uuid
            && !self.origin.is_empty()
            && self.origin.len() <= 2048
            && !self.instance.is_empty()
            && self.instance.len() <= 128
    }
}

fn secret() -> String {
    faster_hex::hex_string(&crate::utils::rand()[..32])
}

fn token_hash(token: &str) -> Result<Vec<u8>> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ErrorCode::Unauthenticated);
    }
    Ok(crate::storage::sha256(token.as_bytes()))
}

fn insert_session(
    transaction: &rusqlite::Transaction<'_>,
    binding: &AdminBinding<'_>,
    family: &str,
    refresh_expiry: u64,
    now: u64,
) -> Result<AdminCredentials> {
    let access_token = secret();
    let refresh_token = (refresh_expiry > now).then(secret);
    let expires_at = now.checked_add(ACCESS_SECONDS).ok_or(ErrorCode::Internal)?;
    let credentials = AdminCredentials {
        access_token,
        refresh_token,
        expires_at,
    };
    insert_credentials(transaction, binding, family, refresh_expiry, &credentials)?;
    Ok(credentials)
}

fn insert_credentials(
    transaction: &rusqlite::Transaction<'_>,
    binding: &AdminBinding<'_>,
    family: &str,
    refresh_expiry: u64,
    credentials: &AdminCredentials,
) -> Result<()> {
    let refresh_hash = credentials
        .refresh_token
        .as_deref()
        .map(token_hash)
        .transpose()?;
    transaction.execute(
        "INSERT INTO library_admin_sessions
         (token_hash, refresh_hash, family_id, player_uuid, origin, instance_id,
          config_revision, expires_at, refresh_expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![token_hash(&credentials.access_token)?, refresh_hash, family, binding.player.to_string(),
            binding.origin, binding.instance, binding.config.fingerprint(), sql_integer(credentials.expires_at)?, sql_integer(refresh_expiry)?],
    ).map_err(|_| ErrorCode::Internal)?;
    Ok(())
}

impl Storage {
    /// Reconcile before publishing reloaded configuration. Revocation survives a
    /// later restoration of an old password/name, so old credentials cannot revive.
    pub fn library_admin_reconcile(&self, config: &LibraryConfig) -> Result<()> {
        self.library_transaction(|transaction| {
            let mut statement = transaction.prepare(
                "SELECT token_hash, player_uuid, config_revision FROM library_admin_sessions WHERE revoked = 0",
            ).map_err(|_| ErrorCode::Internal)?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?))
            }).map_err(|_| ErrorCode::Internal)?;
            for row in rows {
                let (token, uuid, fingerprint) = row.map_err(|_| ErrorCode::Internal)?;
                let unchanged = config.admins.iter().any(|admin| {
                    admin.uuid.to_string() == uuid && admin.fingerprint() == fingerprint
                });
                if !unchanged {
                    transaction.execute(
                        "UPDATE library_admin_sessions SET revoked = 1 WHERE token_hash = ?1", [token],
                    ).map_err(|_| ErrorCode::Internal)?;
                }
            }
            Ok(())
        })
    }

    /// Caller must first successfully verify the configured password and current session.
    pub fn library_admin_issue(
        &self,
        binding: &AdminBinding<'_>,
        remember: bool,
        now: u64,
    ) -> Result<AdminCredentials> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        let refresh_expiry = if remember {
            now.checked_add(REFRESH_SECONDS)
                .ok_or(ErrorCode::Internal)?
        } else {
            now
        };
        self.library_transaction(|transaction| {
            insert_session(transaction, binding, &secret(), refresh_expiry, now)
        })
    }

    pub fn library_admin_authorize(
        &self,
        binding: &AdminBinding<'_>,
        token: &str,
        now: u64,
    ) -> Result<()> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        let hash = token_hash(token)?;
        let valid: bool = self.library_read(|database| {
            database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_admin_sessions WHERE token_hash = ?1
                 AND player_uuid = ?2 AND origin = ?3 AND instance_id = ?4 AND config_revision = ?5
                 AND expires_at > ?6 AND revoked = 0)",
                    params![
                        hash,
                        binding.player.to_string(),
                        binding.origin,
                        binding.instance,
                        binding.config.fingerprint(),
                        sql_integer(now)?
                    ],
                    |row| row.get(0),
                )
                .map_err(|_| ErrorCode::Internal)
        })?;
        if valid {
            Ok(())
        } else {
            Err(ErrorCode::Unauthenticated)
        }
    }

    pub fn library_admin_refresh(
        &self,
        binding: &AdminBinding<'_>,
        token: &str,
        now: u64,
    ) -> Result<AdminCredentials> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        let hash = token_hash(token)?;
        // Return a nested Result so replay-family revocation commits even on rejection.
        self.library_transaction(|transaction| {
            let previous: Option<(String, i64, bool, bool)> = transaction.query_row(
                "SELECT family_id, refresh_expires_at, refresh_used, revoked FROM library_admin_sessions
                 WHERE refresh_hash = ?1 AND player_uuid = ?2 AND origin = ?3
                   AND instance_id = ?4 AND config_revision = ?5",
                params![hash, binding.player.to_string(), binding.origin, binding.instance,
                    binding.config.fingerprint()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(|_| ErrorCode::Internal)?;
            let Some((family, expiry, used, revoked)) = previous else {
                return Ok(Err(ErrorCode::Unauthenticated));
            };
            let expiry = u64::try_from(expiry).map_err(|_| ErrorCode::Internal)?;
            if used || revoked || expiry <= now {
                transaction.execute("UPDATE library_admin_sessions SET revoked = 1 WHERE family_id = ?1", [&family])
                    .map_err(|_| ErrorCode::Internal)?;
                return Ok(Err(ErrorCode::Unauthenticated));
            }
            transaction.execute(
                "UPDATE library_admin_sessions SET refresh_used = 1, revoked = 1 WHERE refresh_hash = ?1", [hash],
            ).map_err(|_| ErrorCode::Internal)?;
            Ok(Ok(insert_session(transaction, binding, &family, expiry, now)?))
        })?
    }

    pub fn library_admin_logout(&self, binding: &AdminBinding<'_>, token: &str) -> Result<()> {
        let hash = token_hash(token)?;
        self.library_transaction(|transaction| {
            transaction.execute(
                "UPDATE library_admin_sessions SET revoked = 1 WHERE family_id IN
                 (SELECT family_id FROM library_admin_sessions WHERE (token_hash = ?1 OR refresh_hash = ?1)
                  AND player_uuid = ?2 AND origin = ?3 AND instance_id = ?4)",
                params![hash, binding.player.to_string(), binding.origin, binding.instance],
            ).map_err(|_| ErrorCode::Internal)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
