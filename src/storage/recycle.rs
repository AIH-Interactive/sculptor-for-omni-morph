//! File placement and durable recovery copies. Catalog membership is always database-driven.
use super::*;
use rusqlite::Connection;
use serde_json::json;

fn io_error(_: std::io::Error) -> ErrorCode {
    ErrorCode::Internal
}

pub(crate) fn copy_durable(source: &Path, target: &Path) -> Result<()> {
    if target.is_file() {
        if fs::read(source).map_err(io_error)? != fs::read(target).map_err(io_error)? {
            return Err(ErrorCode::Conflict);
        }
        return Ok(());
    }
    fs::create_dir_all(target.parent().ok_or(ErrorCode::Internal)?).map_err(io_error)?;
    let staged = target.with_extension(format!("{}.part", crate::service::library::random_id()));
    let result = (|| {
        fs::copy(source, &staged).map_err(io_error)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&staged)
            .map_err(io_error)?
            .sync_all()
            .map_err(io_error)?;
        fs::rename(&staged, target).map_err(io_error)
    })();
    let _ = fs::remove_file(staged);
    result
}

impl Storage {
    pub(crate) fn reconcile_library_recycle(&self) -> Result<()> {
        let _publication = self.lock_blob_files()?;
        self.library_transaction(|transaction| {
            let mut statement = transaction.prepare(
                "SELECT m.model_id FROM library_models m WHERE m.moderation_status = 'DELETED'
                 AND m.published = 1 AND NOT EXISTS(SELECT 1 FROM library_recycle r WHERE r.model_id = m.model_id)"
            ).map_err(sqlite::sql_error)?;
            let ids = statement.query_map([], |row| row.get::<_, String>(0)).map_err(sqlite::sql_error)?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(sqlite::sql_error)?;
            for id in ids {
                self.archive_library(transaction, &id)?;
                transaction.execute(
                    "INSERT INTO library_recycle(model_id, original_status, deleted_at)
                     VALUES (?1, CASE WHEN EXISTS(SELECT 1 FROM library_hash_blocked_models WHERE model_id = ?1)
                     THEN 'BLOCKED' ELSE 'ACTIVE' END, ?2)",
                    rusqlite::params![id, super::library::sql_integer(now_seconds())?],
                ).map_err(sqlite::sql_error)?;
            }
            Ok(())
        })?;
        self.library_read(|database| {
            let mut statement = database
                .prepare("SELECT model_id, purged FROM library_recycle")
                .map_err(sqlite::sql_error)?;
            let records = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })
                .map_err(sqlite::sql_error)?;
            for record in records {
                let (id, purged) = record.map_err(sqlite::sql_error)?;
                self.clean_library_files(database, &id)?;
                let folder = self.library_recycle_path(&id)?;
                if purged && folder.is_dir() {
                    fs::remove_dir_all(folder).map_err(io_error)?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn separate_wardrobe_files(&self) -> Result<()> {
        self.library_read(|database| {
            for entry in fs::read_dir(&self.models).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                if !entry.file_type().map_err(io_error)?.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let legacy = name.strip_suffix(".moon").is_some_and(|id| Uuid::parse_str(id).is_ok());
                if !legacy && super::library::validate_hash("RAW_SHA256", &name).is_err() {
                    continue;
                }
                let library: bool = database.query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_revisions WHERE raw_sha256 = ?1
                     UNION ALL SELECT 1 FROM library_images WHERE hash = ?1)",
                    [&name], |row| row.get(0),
                ).map_err(sqlite::sql_error)?;
                let wardrobe: bool = database.query_row(
                    "SELECT EXISTS(SELECT 1 FROM avatars a WHERE lower(hex(a.raw_sha256)) = ?1
                     AND NOT EXISTS(SELECT 1 FROM library_resource_bindings b WHERE
                     b.resource_owner = a.owner_uuid AND b.asset_id = a.asset_id AND b.revision = a.revision))",
                    [&name], |row| row.get(0),
                ).map_err(sqlite::sql_error)?;
                if legacy || wardrobe || !library {
                    copy_durable(&entry.path(), &self.wardrobe.join(&name))?;
                }
                if !library {
                    fs::remove_file(entry.path()).map_err(io_error)?;
                }
            }
            Ok(())
        })
    }

    pub(super) fn archive_wardrobe(
        &self,
        owner: Uuid,
        request: &Delete,
        history: &BTreeMap<u64, StoredAvatar>,
    ) -> Result<()> {
        let operation =
            Uuid::parse_str(&request.operation_id).map_err(|_| ErrorCode::InvalidMessage)?;
        let folder = self
            .recycle
            .join("wardrobe")
            .join(owner.to_string())
            .join(operation.to_string());
        fs::create_dir_all(&folder).map_err(io_error)?;
        for (revision, stored) in history {
            let avatar = AvatarDescriptor::decode(stored.descriptor.as_slice())
                .map_err(|_| ErrorCode::Internal)?;
            let source = self.blob_path(&avatar.raw_sha256)?;
            if source.is_file() {
                copy_durable(
                    &source,
                    &folder.join(faster_hex::hex_string(&avatar.raw_sha256)),
                )?;
            } else if !stored.tombstone {
                return Err(ErrorCode::NotFound);
            }
            let descriptor = folder.join(format!("revision-{revision}.pb"));
            fs::write(&descriptor, &stored.descriptor).map_err(io_error)?;
            fs::OpenOptions::new()
                .write(true)
                .open(descriptor)
                .map_err(io_error)?
                .sync_all()
                .map_err(io_error)?;
        }
        let metadata = json!({"owner": owner, "asset_id": request.asset_id,
            "expected_revision": request.expected_revision, "operation_id": request.operation_id,
            "deleted_at": now_seconds(), "descriptor_format": "AvatarDescriptor protobuf"});
        let target = folder.join("metadata.json");
        fs::write(
            &target,
            serde_json::to_vec_pretty(&metadata).map_err(|_| ErrorCode::Internal)?,
        )
        .map_err(io_error)?;
        fs::OpenOptions::new()
            .write(true)
            .open(target)
            .map_err(io_error)?
            .sync_all()
            .map_err(io_error)
    }

    pub(super) fn clean_deleted_wardrobe(&self, owner: Uuid, asset: &str) -> Result<()> {
        self.library_read(|database| {
            let mut statement = database.prepare(
                "SELECT DISTINCT lower(hex(raw_sha256)) FROM avatars a WHERE tombstone = 1
                 AND owner_uuid = ?1 AND asset_id = ?2
                 AND NOT EXISTS(SELECT 1 FROM avatars live WHERE live.raw_sha256 = a.raw_sha256 AND live.tombstone = 0
                   AND NOT EXISTS(SELECT 1 FROM library_resource_bindings b WHERE b.resource_owner = live.owner_uuid
                     AND b.asset_id = live.asset_id AND b.revision = live.revision))"
            ).map_err(sqlite::sql_error)?;
            let hashes = statement.query_map(rusqlite::params![owner.to_string(), asset], |row| row.get::<_, String>(0)).map_err(sqlite::sql_error)?;
            for hash in hashes {
                let file = self.wardrobe.join(hash.map_err(sqlite::sql_error)?);
                if file.is_file() {
                    fs::remove_file(file).map_err(io_error)?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn library_recycle_path(&self, id: &str) -> Result<PathBuf> {
        let uuid = Uuid::parse_str(id).map_err(|_| ErrorCode::InvalidMessage)?;
        Ok(self.recycle.join("library").join(uuid.to_string()))
    }

    pub(crate) fn library_files(database: &Connection, id: &str) -> Result<Vec<String>> {
        let mut statement = database
            .prepare(
                "SELECT raw_sha256 FROM library_revisions WHERE model_id = ?1
             UNION SELECT hash FROM library_images WHERE model_id = ?1",
            )
            .map_err(sqlite::sql_error)?;
        let rows = statement
            .query_map([id], |row| row.get::<_, String>(0))
            .map_err(sqlite::sql_error)?;
        rows.map(|row| row.map_err(sqlite::sql_error)).collect()
    }

    pub(crate) fn archive_library(&self, database: &Connection, id: &str) -> Result<()> {
        let folder = self.library_recycle_path(id)?;
        fs::create_dir_all(&folder).map_err(io_error)?;
        for hash in Self::library_files(database, id)? {
            super::library::validate_hash("RAW_SHA256", &hash)?;
            let source = self.models.join(&hash);
            let target = folder.join(&hash);
            if source.is_file() {
                copy_durable(&source, &target)?;
            } else if !target.is_file() {
                return Err(ErrorCode::NotFound);
            }
        }
        let snapshot: String = database.query_row(
            "SELECT json_object('model_id', model_id, 'uploader_uuid', uploader_uuid,
             'display_name', display_name, 'internal_name', internal_name,
             'authors', json(authors_json), 'visibility', visibility,
             'requires_authorization', requires_authorization, 'moderation_status', moderation_status,
             'current_revision', current_revision, 'policy_revision', policy_revision)
             FROM library_models WHERE model_id = ?1", [id], |row| row.get(0),
        ).map_err(sqlite::sql_error)?;
        // Do not overwrite the original pre-delete status during retry/recovery.
        let target = folder.join("metadata.json");
        if !target.exists() {
            fs::write(&target, snapshot).map_err(io_error)?;
            fs::OpenOptions::new()
                .write(true)
                .open(target)
                .map_err(io_error)?
                .sync_all()
                .map_err(io_error)?;
        }
        Ok(())
    }

    pub(crate) fn clean_library_files(&self, database: &Connection, id: &str) -> Result<()> {
        for hash in Self::library_files(database, id)? {
            let referenced: bool = database.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_revisions r JOIN library_models m USING(model_id)
                 WHERE r.raw_sha256 = ?1 AND m.moderation_status != 'DELETED'
                 UNION ALL SELECT 1 FROM library_images i JOIN library_models m USING(model_id)
                 WHERE i.hash = ?1 AND m.moderation_status != 'DELETED')", [&hash], |row| row.get(0),
            ).map_err(sqlite::sql_error)?;
            let path = self.models.join(hash);
            if !referenced && path.is_file() {
                fs::remove_file(path).map_err(io_error)?;
            }
        }
        Ok(())
    }
}
