//! Durable library data, independent of the legacy manifest's rewritten rows.
use super::{ErrorCode, Result, Storage, sqlite::sql_error};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;
use uuid::Uuid;

pub(super) const SCHEMA_VERSION: u32 = 13;

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
            "pre-library-{}.db",
            faster_hex::hex_string(&crate::utils::rand()[..16])
        ));
        database.backup("main", &backup, None).map_err(sql_error)?;
    }
    // SQLite requires FK enforcement to be disabled outside the table-rebuild transaction.
    // Restore the caller's setting on both success and rollback.
    let foreign_keys: bool = database
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(sql_error)?;
    database
        .pragma_update(None, "foreign_keys", false)
        .map_err(sql_error)?;
    let result = migrate_transaction(database, version);
    database
        .pragma_update(None, "foreign_keys", foreign_keys)
        .map_err(sql_error)?;
    result
}

fn migrate_transaction(database: &mut Connection, version: u32) -> Result<()> {
    let transaction = database.transaction().map_err(sql_error)?;
    if version < 2 {
        transaction
            .execute_batch(include_str!("library.sql"))
            .map_err(sql_error)?;
    }
    if version < 3 {
        transaction
            .execute_batch(include_str!("library_distribution.sql"))
            .map_err(sql_error)?;
    }
    if version < 4 {
        transaction
            .execute_batch(include_str!("library_reports.sql"))
            .map_err(sql_error)?;
        transaction
            .execute_batch(include_str!("library_wardrobe.sql"))
            .map_err(sql_error)?;
        transaction
            .execute_batch(include_str!("library_appeals.sql"))
            .map_err(sql_error)?;
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_recycle (
         model_id TEXT PRIMARY KEY REFERENCES library_models(model_id),
         original_status TEXT NOT NULL, deleted_at INTEGER NOT NULL,
         purged INTEGER NOT NULL DEFAULT 0 CHECK(purged IN (0,1)));"
    ).map_err(sql_error)?;
    if version < 6 {
        transaction
            .execute_batch(include_str!("library_collection.sql"))
            .map_err(sql_error)?;
    }
    if version < 7 {
        let required: bool = transaction.query_row(
            "SELECT \"notnull\" FROM pragma_table_info('library_models') WHERE name = 'model_type'",
            [], |row| row.get(0),
        ).map_err(sql_error)?;
        if required {
            // Preserve every original column, index, dependent FK and effective-policy view.
            let schema = include_str!("library.sql")
                .split("CREATE TABLE library_revisions")
                .next()
                .ok_or(ErrorCode::Internal)?
                .replace(
                    "CREATE TABLE library_models (",
                    "CREATE TABLE library_models_new (",
                )
                .replace("model_type TEXT NOT NULL CHECK", "model_type TEXT CHECK");
            let (table, indexes) = schema
                .split_once("CREATE INDEX library_public")
                .ok_or(ErrorCode::Internal)?;
            transaction.execute_batch(table).map_err(sql_error)?;
            transaction
                .execute_batch(
                    "INSERT INTO library_models_new SELECT * FROM library_models;
                 DROP VIEW library_effective_models;
                 DROP TABLE library_models;
                 ALTER TABLE library_models_new RENAME TO library_models;",
                )
                .map_err(sql_error)?;
            transaction
                .execute_batch(&format!("CREATE INDEX library_public{indexes}"))
                .map_err(sql_error)?;
            let (_, view) = include_str!("library_wardrobe.sql")
                .split_once("CREATE VIEW library_effective_models AS")
                .ok_or(ErrorCode::Internal)?;
            transaction
                .execute_batch(&format!("CREATE VIEW library_effective_models AS{view}"))
                .map_err(sql_error)?;
        }
        transaction
            .execute_batch(include_str!("outfit.sql"))
            .map_err(sql_error)?;
    }
    if version < 8 {
        transaction
            .execute_batch(include_str!("outfit_transfers.sql"))
            .map_err(sql_error)?;
    }
    if version < 9 {
        transaction
            .execute_batch(include_str!("outfit_resources.sql"))
            .map_err(sql_error)?;
    }
    if version < 10 {
        transaction
            .execute_batch(include_str!("outfit_private_sources.sql"))
            .map_err(sql_error)?;
    }
    if version < 11 {
        transaction
            .execute_batch(include_str!("outfit_uploads.sql"))
            .map_err(sql_error)?;
    }
    if version < 12 {
        transaction
            .execute_batch(include_str!("library_images_configurable.sql"))
            .map_err(sql_error)?;
    }
    if version < 13 {
        migrate_psf_types(&transaction)?;
    }
    let violations: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if violations {
        return Err(ErrorCode::Internal);
    }
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)
}

/// Rebuild only the two format CHECKs, retaining their actual installed columns,
/// indexes, triggers and dependent view instead of recreating an older schema.
fn migrate_psf_types(transaction: &Transaction<'_>) -> Result<()> {
    let mut tables = Vec::new();
    for table in ["library_models", "outfit_local_resources"] {
        let schema: String = transaction
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if schema.contains("'PSF'") {
            continue;
        }
        let legacy = "'FIGURA','YSM_PLAIN','YSM_ENCRYPTED'";
        if !schema.contains(legacy) {
            return Err(ErrorCode::Internal);
        }
        let fields = schema.find('(').ok_or(ErrorCode::Internal)?;
        let definition =
            schema[fields..].replace(legacy, "'FIGURA','YSM_PLAIN','YSM_ENCRYPTED','PSF'");
        let mut statement = transaction
            .prepare(
                "SELECT sql FROM sqlite_master WHERE tbl_name = ?1
             AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type, name",
            )
            .map_err(sql_error)?;
        let objects = statement
            .query_map([table], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        tables.push((table, definition, objects));
    }
    if tables.is_empty() {
        return Ok(());
    }
    let view: Option<String> = transaction.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'library_effective_models'",
        [], |row| row.get(0),
    ).optional().map_err(sql_error)?;
    if view.is_some() {
        transaction
            .execute_batch("DROP VIEW library_effective_models;")
            .map_err(sql_error)?;
    }
    for (table, definition, objects) in tables {
        let replacement = format!("{table}_psf13");
        transaction
            .execute_batch(&format!(
                "CREATE TABLE {replacement} {definition};
             INSERT INTO {replacement} SELECT * FROM {table};
             DROP TABLE {table};
             ALTER TABLE {replacement} RENAME TO {table};"
            ))
            .map_err(sql_error)?;
        for object in objects {
            transaction.execute_batch(&object).map_err(sql_error)?;
        }
    }
    if let Some(view) = view {
        transaction.execute_batch(&view).map_err(sql_error)?;
    }
    Ok(())
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
                    "SELECT EXISTS(SELECT 1 FROM library_active_blocked_hashes
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
pub(crate) mod tests;

#[cfg(test)]
mod psf_migration_tests {
    use super::*;

    #[test]
    fn psf_upgrade_preserves_legacy_rows_columns_indexes_triggers_foreign_keys_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.sqlite3");
        let mut database = Connection::open(&path).unwrap();
        database.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        database
            .execute_batch(
                &include_str!("library.sql").replace("'YSM_ENCRYPTED','PSF'", "'YSM_ENCRYPTED'"),
            )
            .unwrap();
        database
            .execute_batch(
                &include_str!("outfit_resources.sql")
                    .replace("'YSM_ENCRYPTED','PSF'", "'YSM_ENCRYPTED'"),
            )
            .unwrap();
        database.execute_batch(
            "ALTER TABLE library_models ADD COLUMN resource_kind TEXT NOT NULL DEFAULT 'AVATAR';
             CREATE VIEW library_effective_models AS SELECT * FROM library_models;
             CREATE TABLE migration_fk (model_id TEXT REFERENCES library_models(model_id));
             CREATE TABLE migration_audit (model_id TEXT);
             CREATE TRIGGER migration_trigger AFTER UPDATE ON library_models
             BEGIN INSERT INTO migration_audit VALUES (NEW.model_id); END;"
        ).unwrap();
        for kind in ["FIGURA", "YSM_PLAIN", "YSM_ENCRYPTED"] {
            database.execute(
                "INSERT INTO library_models
                 (model_id,uploader_uuid,uploader_name_snapshot,display_name,internal_name,authors_json,
                  model_type,category_id,description,visibility,requires_authorization,moderation_status,
                  current_revision,policy_revision,created_at,updated_at,published)
                 VALUES (?1,'owner','Owner',?1,?1,'[]',?1,'general','','PUBLIC',0,'ACTIVE',1,1,1,1,1)",
                [kind],
            ).unwrap();
            database
                .execute("INSERT INTO migration_fk VALUES (?1)", [kind])
                .unwrap();
            database
                .execute(
                    "INSERT INTO outfit_local_resources VALUES ('owner',?1,?2,?1,'{}')",
                    params![kind, "a".repeat(64)],
                )
                .unwrap();
        }
        database.pragma_update(None, "user_version", 12).unwrap();
        migrate(&mut database, &path, 12).unwrap();
        assert!(database
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM library_effective_models", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            3
        );
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM outfit_local_resources", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        database
            .execute(
                "UPDATE library_models SET model_type='PSF' WHERE model_id='FIGURA'",
                [],
            )
            .unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM migration_audit", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        database
            .execute(
                "INSERT INTO outfit_local_resources VALUES ('owner','psf',?1,'PSF','{}')",
                ["b".repeat(64)],
            )
            .unwrap();
        drop(database);
        let mut reopened = Connection::open(&path).unwrap();
        migrate(&mut reopened, &path, 13).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT resource_kind FROM library_models WHERE model_id='FIGURA'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "AVATAR"
        );
        assert_eq!(
            reopened
                .query_row("SELECT COUNT(*) FROM migration_fk", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(reopened.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'library_%'", [], |row| row.get::<_, i64>(0)).unwrap() > 0, true);
    }
}
