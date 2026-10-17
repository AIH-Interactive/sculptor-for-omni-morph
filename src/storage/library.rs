//! Durable library data, independent of the legacy manifest's rewritten rows.
use super::{ErrorCode, Result, Storage, sqlite::sql_error};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;
use uuid::Uuid;

pub(super) const SCHEMA_VERSION: u32 = 3;

pub(crate) fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| ErrorCode::InvalidMessage)
}

pub(super) fn migrate(database: &mut Connection, path: &Path, version: u32) -> Result<()> {
    if version > SCHEMA_VERSION {
        return Err(ErrorCode::UnsupportedVersion);
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version > 0 {
        // Unique backup names preserve the first usable pre-upgrade snapshot on retry.
        let backup = path.with_extension(format!(
            "pre-library-{}.sqlite3",
            faster_hex::hex_string(&crate::utils::rand()[..16])
        ));
        database.backup("main", &backup, None).map_err(sql_error)?;
    }
    let transaction = database.transaction().map_err(sql_error)?;
    if version < 2 {
        transaction
            .execute_batch(include_str!("library.sql"))
            .map_err(sql_error)?;
    }
    transaction
        .execute_batch(include_str!("library_distribution.sql"))
        .map_err(sql_error)?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)
}

impl Storage {
    /// Call on a bounded blocking worker. Never acquire manifest locks from this closure.
    pub(crate) fn library_read<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        read(&database)
    }

    pub(crate) fn library_transaction<T>(
        &self,
        change: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut database = self.database.lock().map_err(|_| ErrorCode::Internal)?;
        let transaction = database.transaction().map_err(sql_error)?;
        let result = change(&transaction)?;
        transaction.commit().map_err(sql_error)?;
        Ok(result)
    }

    pub fn library_hash_blocked(&self, kind: &str, hash: &str) -> Result<bool> {
        validate_hash(kind, hash)?;
        self.library_read(|database| {
            database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_blocked_hashes
                 WHERE hash_kind = ?1 AND verified_hash = ?2 AND active = 1)",
                    params![kind, hash],
                    |row| row.get(0),
                )
                .map_err(sql_error)
        })
    }
}

pub fn validate_hash(kind: &str, hash: &str) -> Result<()> {
    if !matches!(
        kind,
        "RAW_SHA256" | "NBT_SHA256" | "YSM_CIPHERTEXT_SHA256" | "LEGACY_FIGURA_BASE64_SHA256"
    ) || hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok(())
}

/// Run inside the same transaction as its model/binding changes.
pub(crate) fn operation(
    transaction: &Transaction<'_>,
    actor: Uuid,
    id: Uuid,
    fingerprint: &[u8],
    now: u64,
    change: impl FnOnce(&Transaction<'_>) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let previous: Option<(Vec<u8>, Vec<u8>)> = transaction.query_row(
        "SELECT fingerprint, result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
        params![actor.to_string(), id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_error)?;
    if let Some((prior_fingerprint, result)) = previous {
        return if prior_fingerprint == fingerprint {
            Ok(result)
        } else {
            Err(ErrorCode::Conflict)
        };
    }
    let result = change(transaction)?;
    transaction
        .execute(
            "INSERT INTO library_operations VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                actor.to_string(),
                id.to_string(),
                fingerprint,
                result,
                sql_integer(now.checked_add(86400).ok_or(ErrorCode::InvalidMessage)?)?
            ],
        )
        .map_err(sql_error)?;
    Ok(result)
}

/// Only the service, after validating an admin session, may call this helper.
pub(crate) fn set_blocked(
    transaction: &Transaction<'_>,
    model: &str,
    actor: Uuid,
    expected_revision: u64,
    blocked: bool,
    reason: &str,
    now: u64,
) -> Result<()> {
    if reason.len() > 512 {
        return Err(ErrorCode::LimitExceeded);
    }
    let updated = transaction
        .execute(
            "UPDATE library_models SET visibility = 'PRIVATE',
         moderation_status = CASE WHEN moderation_status = 'DELETED' THEN 'DELETED'
             WHEN ?1 THEN 'BLOCKED' ELSE 'ACTIVE' END,
         policy_revision = policy_revision + 1, updated_at = ?2
         WHERE model_id = ?3 AND policy_revision = ?4",
            params![
                blocked,
                sql_integer(now)?,
                model,
                sql_integer(expected_revision)?
            ],
        )
        .map_err(sql_error)?;
    if updated == 0 {
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_models WHERE model_id = ?1)",
                [model],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        return Err(if exists {
            ErrorCode::Conflict
        } else {
            ErrorCode::NotFound
        });
    }
    if blocked {
        transaction
            .execute(
                "INSERT INTO library_blocked_hashes
             SELECT hash_kind, verified_hash, model_id, 1, ?2, ?3, ?4
             FROM library_revision_hashes WHERE model_id = ?1
             ON CONFLICT(hash_kind, verified_hash, blocking_model_id)
             DO UPDATE SET active = 1, reason = excluded.reason,
                 actor_uuid = excluded.actor_uuid, created_at = excluded.created_at",
                params![model, reason, actor.to_string(), sql_integer(now)?],
            )
            .map_err(sql_error)?;
    } else {
        transaction
            .execute(
                "UPDATE library_blocked_hashes SET active = 0 WHERE blocking_model_id = ?1",
                [model],
            )
            .map_err(sql_error)?;
    }
    transaction.execute(
        "UPDATE library_models SET visibility = 'PRIVATE', policy_revision = policy_revision + 1,
             updated_at = ?2
         WHERE model_id != ?1 AND moderation_status != 'DELETED'
           AND model_id IN (
             SELECT copies.model_id FROM library_revision_hashes source
             JOIN library_revision_hashes copies ON copies.hash_kind = source.hash_kind
                 AND copies.verified_hash = source.verified_hash
             WHERE source.model_id = ?1)",
        params![model, sql_integer(now)?],
    ).map_err(sql_error)?;
    transaction
        .execute(
            "INSERT INTO library_audit(actor_uuid, action, model_id, created_at, result)
         VALUES (?1, ?2, ?3, ?4, 'OK')",
            params![
                actor.to_string(),
                if blocked { "BLOCK" } else { "UNBLOCK" },
                model,
                sql_integer(now)?
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
