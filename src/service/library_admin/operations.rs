//! Credential retries without storing plaintext tokens in an operation journal.
use super::*;
use crate::storage::library;
use ring::hmac;
use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

static OPERATION_KEY: LazyLock<hmac::Key> =
    LazyLock::new(|| hmac::Key::new(hmac::HMAC_SHA256, &crate::utils::rand()[..32]));
static LOGIN_ATTEMPTS: LazyLock<Mutex<HashMap<(String, Uuid), (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct CredentialOperation {
    id: Uuid,
    fingerprint: Vec<u8>,
    access: String,
    refresh: String,
    family: String,
}

pub fn secret_fingerprint(value: &str) -> Vec<u8> {
    hmac::sign(
        &OPERATION_KEY,
        &[b"request-secret".as_slice(), value.as_bytes()].concat(),
    )
    .as_ref()
    .to_vec()
}

impl CredentialOperation {
    pub fn new(
        binding: &AdminBinding<'_>,
        action: &str,
        operation: &str,
        request: &[u8],
    ) -> Result<Self> {
        let id = crate::service::resource::operation_id(operation)?;
        let scope = serde_json::to_vec(&(
            binding.player.to_string(),
            binding.origin,
            binding.instance,
            binding.config.fingerprint(),
            action,
            operation,
        ))
        .map_err(|_| ErrorCode::Internal)?;
        let fingerprint = hmac::sign(&OPERATION_KEY, &[scope.as_slice(), request].concat())
            .as_ref()
            .to_vec();
        let derive = |purpose: &[u8]| {
            faster_hex::hex_string(
                hmac::sign(&OPERATION_KEY, &[purpose, fingerprint.as_slice()].concat()).as_ref(),
            )
        };
        Ok(Self {
            id,
            access: derive(b"access"),
            refresh: derive(b"refresh"),
            family: derive(b"family"),
            fingerprint,
        })
    }

    fn credentials(&self, metadata: &CredentialMetadata) -> AdminCredentials {
        AdminCredentials {
            access_token: self.access.clone(),
            expires_at: metadata.expires_at,
            refresh_token: metadata.remember.then(|| self.refresh.clone()),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct CredentialMetadata {
    expires_at: u64,
    refresh_expires_at: u64,
    remember: bool,
}

fn replay(
    database: &Connection,
    binding: &AdminBinding<'_>,
    operation: &CredentialOperation,
    now: u64,
) -> Result<Option<AdminCredentials>> {
    let previous: Option<(Vec<u8>, Vec<u8>)> = database.query_row(
        "SELECT fingerprint, result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
        params![binding.player.to_string(), operation.id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(|_| ErrorCode::Internal)?;
    let Some((fingerprint, bytes)) = previous else {
        return Ok(None);
    };
    if fingerprint != operation.fingerprint {
        return Err(ErrorCode::Conflict);
    }
    let metadata: CredentialMetadata =
        serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)?;
    let credentials = operation.credentials(&metadata);
    let valid: bool = database.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_admin_sessions WHERE token_hash = ?1 AND revoked = 0
         AND player_uuid = ?2 AND origin = ?3 AND instance_id = ?4 AND config_revision = ?5 AND expires_at > ?6)",
        params![token_hash(&credentials.access_token)?, binding.player.to_string(), binding.origin,
            binding.instance, binding.config.fingerprint(), sql_integer(now)?], |row| row.get(0),
    ).map_err(|_| ErrorCode::Internal)?;
    if !valid {
        return Err(ErrorCode::Unauthenticated);
    }
    Ok(Some(credentials))
}

pub fn admit_login(binding: &AdminBinding<'_>) -> Result<()> {
    let mut attempts = LOGIN_ATTEMPTS.lock().map_err(|_| ErrorCode::Internal)?;
    attempts.retain(|_, (started, _)| started.elapsed() < Duration::from_secs(60));
    let key = (binding.instance.to_owned(), binding.player);
    if attempts.len() >= 4096 && !attempts.contains_key(&key) {
        return Err(ErrorCode::LimitExceeded);
    }
    let (_, count) = attempts.entry(key).or_insert((Instant::now(), 0));
    if *count >= 5 {
        return Err(ErrorCode::LimitExceeded);
    }
    *count += 1;
    Ok(())
}

impl Storage {
    pub fn library_admin_replay(
        &self,
        binding: &AdminBinding<'_>,
        operation: &CredentialOperation,
        now: u64,
    ) -> Result<Option<AdminCredentials>> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        self.library_read(|database| replay(database, binding, operation, now))
    }

    /// Password verification must finish before this transaction is allowed.
    pub fn library_admin_issue_operation(
        &self,
        binding: &AdminBinding<'_>,
        operation: &CredentialOperation,
        remember: bool,
        now: u64,
    ) -> Result<AdminCredentials> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        self.library_transaction(|transaction| {
            if let Some(credentials) = replay(transaction, binding, operation, now)? {
                return Ok(credentials);
            }
            let metadata = CredentialMetadata {
                expires_at: now + ACCESS_SECONDS,
                refresh_expires_at: if remember { now + REFRESH_SECONDS } else { now },
                remember,
            };
            let credentials = operation.credentials(&metadata);
            library::operation(
                transaction,
                binding.player,
                operation.id,
                &operation.fingerprint,
                now,
                |transaction| {
                    insert_credentials(
                        transaction,
                        binding,
                        &operation.family,
                        metadata.refresh_expires_at,
                        &credentials,
                    )?;
                    serde_json::to_vec(&metadata).map_err(|_| ErrorCode::Internal)
                },
            )?;
            Ok(credentials)
        })
    }

    pub fn library_admin_refresh_operation(
        &self,
        binding: &AdminBinding<'_>,
        operation: &CredentialOperation,
        token: &str,
        now: u64,
    ) -> Result<AdminCredentials> {
        if !binding.valid() {
            return Err(ErrorCode::Forbidden);
        }
        let token = token_hash(token)?;
        // A rejected refresh can still durably revoke a reused token family.
        self.library_transaction(|transaction| {
            if let Some(credentials) = replay(transaction, binding, operation, now)? {
                return Ok(Ok(credentials));
            }
            let previous: Option<(String, i64, bool, bool)> = transaction.query_row(
                "SELECT family_id, refresh_expires_at, refresh_used, revoked FROM library_admin_sessions
                 WHERE refresh_hash = ?1 AND player_uuid = ?2 AND origin = ?3 AND instance_id = ?4 AND config_revision = ?5",
                params![token, binding.player.to_string(), binding.origin, binding.instance, binding.config.fingerprint()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional().map_err(|_| ErrorCode::Internal)?;
            let Some((family, expiry, used, revoked)) = previous else {
                return Ok(Err(ErrorCode::Unauthenticated));
            };
            let expiry = u64::try_from(expiry).map_err(|_| ErrorCode::Internal)?;
            if used || revoked || expiry <= now {
                transaction.execute("UPDATE library_admin_sessions SET revoked = 1 WHERE family_id = ?1", [&family]).map_err(|_| ErrorCode::Internal)?;
                return Ok(Err(ErrorCode::Unauthenticated));
            }
            let metadata = CredentialMetadata {
                expires_at: now + ACCESS_SECONDS,
                refresh_expires_at: expiry,
                remember: true,
            };
            let credentials = operation.credentials(&metadata);
            library::operation(transaction, binding.player, operation.id, &operation.fingerprint, now, |transaction| {
                transaction.execute("UPDATE library_admin_sessions SET refresh_used = 1, revoked = 1 WHERE refresh_hash = ?1", [token]).map_err(|_| ErrorCode::Internal)?;
                insert_credentials(transaction, binding, &family, expiry, &credentials)?;
                serde_json::to_vec(&metadata).map_err(|_| ErrorCode::Internal)
            })?;
            Ok(Ok(credentials))
        })?
    }

    pub fn library_admin_logout_operation(
        &self,
        binding: &AdminBinding<'_>,
        operation: &CredentialOperation,
        token: &str,
        now: u64,
    ) -> Result<()> {
        let token = token_hash(token)?;
        self.library_transaction(|transaction: &Transaction<'_>| {
            library::operation(transaction, binding.player, operation.id, &operation.fingerprint, now, |transaction| {
                transaction.execute("UPDATE library_admin_sessions SET revoked = 1 WHERE family_id IN
                    (SELECT family_id FROM library_admin_sessions WHERE token_hash = ?1 AND player_uuid = ?2 AND origin = ?3 AND instance_id = ?4)",
                    params![token, binding.player.to_string(), binding.origin, binding.instance]).map_err(|_| ErrorCode::Internal)?;
                Ok(b"{}".to_vec())
            })?;
            Ok(())
        })
    }
}
