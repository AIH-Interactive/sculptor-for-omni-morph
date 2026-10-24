//! Recoverable legacy filename replacement, committed with its native/outfit projection.
use super::*;
use crate::service::outfit_limits::OutfitLimits;
use rusqlite::{Transaction, params};
use std::io::Read;

const PREFIX: &str = "legacy_publication:";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    operation: Uuid,
    had_previous: bool,
}

fn key(owner: Uuid) -> String {
    format!("{PREFIX}{owner}")
}

fn copy_synced(source: &Path, target: &Path) -> Result<()> {
    fs::copy(source, target).map_err(|_| ErrorCode::Internal)?;
    fs::OpenOptions::new()
        .write(true)
        .open(target)
        .and_then(|file| file.sync_all())
        .map_err(|_| ErrorCode::Internal)
}

pub(super) fn finish(transaction: &Transaction<'_>, owner: Uuid, operation: Uuid) -> Result<()> {
    let changed = transaction
        .execute(
            "DELETE FROM metadata WHERE key = ?1 AND json_extract(value, '$.operation') = ?2",
            params![key(owner), operation.to_string()],
        )
        .map_err(sqlite::sql_error)?;
    if changed != 1 {
        return Err(ErrorCode::Conflict);
    }
    Ok(())
}

impl Storage {
    fn legacy_publication_folder(&self, owner: Uuid, operation: Uuid) -> PathBuf {
        self.recycle
            .join("legacy-publications")
            .join(owner.to_string())
            .join(operation.to_string())
    }

    fn clean_legacy_publication(&self, owner: Uuid, pending: &Pending) {
        let folder = self.legacy_publication_folder(owner, pending.operation);
        for name in ["previous.moon", "next.moon"] {
            let path = folder.join(name);
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("Legacy publication file cleanup remains pending");
                }
            }
        }
        if let Err(error) = fs::remove_dir(folder) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("Legacy publication directory cleanup remains pending");
            }
        }
    }

    fn recover_legacy_publications_locked(&self) -> Result<()> {
        let pending = self.library_read(|database| {
            let mut statement = database.prepare(
                "SELECT key, value FROM metadata WHERE key GLOB 'legacy_publication:*' ORDER BY key",
            ).map_err(sqlite::sql_error)?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .map_err(sqlite::sql_error)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sqlite::sql_error)
        })?;
        for (saved_key, json) in pending {
            let owner = saved_key
                .strip_prefix(PREFIX)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or(ErrorCode::Internal)?;
            let pending: Pending = serde_json::from_str(&json).map_err(|_| ErrorCode::Internal)?;
            let folder = self.legacy_publication_folder(owner, pending.operation);
            let target = crate::service::resource::legacy_path(owner, false);
            if pending.had_previous {
                // Keep the backup until its database marker is removed, so recovery
                // itself can be interrupted and safely repeated.
                copy_synced(&folder.join("previous.moon"), &folder.join("next.moon"))?;
                fs::rename(folder.join("next.moon"), &target).map_err(|_| ErrorCode::Internal)?;
            } else if let Err(error) = fs::remove_file(&target) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(ErrorCode::Internal);
                }
            }
            self.library_transaction(|transaction| finish(transaction, owner, pending.operation))?;
            self.clean_legacy_publication(owner, &pending);
        }
        Ok(())
    }

    pub(super) fn recover_legacy_publications(&self) -> Result<()> {
        let _publication = self.lock_blob_files()?;
        self.recover_legacy_publications_locked()
    }

    pub(crate) fn publish_legacy_file(
        &self,
        staged: &Path,
        avatar: AvatarDescriptor,
        limits: &OutfitLimits,
    ) -> Result<AvatarDescriptor> {
        let owner = Uuid::parse_str(&avatar.owner).map_err(|_| ErrorCode::InvalidMessage)?;
        let operation = Uuid::parse_str(&crate::service::resource::new_operation_id())
            .map_err(|_| ErrorCode::Internal)?;
        let _publication = self.lock_blob_files()?;
        self.recover_legacy_publications_locked()?;
        self.publish_staged_locked(staged, &avatar.raw_sha256, false)?;
        let target = crate::service::resource::legacy_path(owner, false);
        let folder = self.legacy_publication_folder(owner, operation);
        let pending = Pending {
            operation,
            had_previous: target.exists(),
        };
        let prepare = || {
            fs::create_dir_all(target.parent().ok_or(ErrorCode::Internal)?)
                .map_err(|_| ErrorCode::Internal)?;
            fs::create_dir_all(&folder).map_err(|_| ErrorCode::Internal)?;
            if pending.had_previous {
                copy_synced(&target, &folder.join("previous.moon"))?;
            }
            let json = serde_json::to_string(&pending).map_err(|_| ErrorCode::Internal)?;
            self.library_transaction(|transaction| {
                transaction
                    .execute(
                        "INSERT INTO metadata VALUES (?1, ?2)",
                        params![key(owner), json],
                    )
                    .map_err(sqlite::sql_error)?;
                Ok(())
            })?;
            copy_synced(
                &self.blob_path(&avatar.raw_sha256)?,
                &folder.join("next.moon"),
            )?;
            fs::rename(folder.join("next.moon"), &target).map_err(|_| ErrorCode::Internal)
        };
        if let Err(error) = prepare() {
            self.recover_legacy_publications_locked()?;
            self.clean_legacy_publication(owner, &pending);
            return Err(error);
        }
        match self.import_legacy_publication(avatar, true, limits, Some(operation)) {
            Ok(avatar) => {
                self.clean_legacy_publication(owner, &pending);
                Ok(avatar)
            }
            Err(error) => {
                self.recover_legacy_publications_locked()?;
                Err(error)
            }
        }
    }

    /// Called while holding the publication lock. Reject a snapshot taken from
    /// a transient replacement that was rolled back before this reader acquired it.
    pub(crate) fn legacy_file_matches(
        &self,
        owner: Uuid,
        expected: &[u8],
        limit: u64,
    ) -> Result<bool> {
        let path = crate::service::resource::legacy_read_path(owner, false);
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(ErrorCode::Internal),
        };
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 8192];
        let mut count = 0u64;
        loop {
            let read = file.read(&mut buffer).map_err(|_| ErrorCode::Internal)?;
            if read == 0 {
                break;
            }
            count = count
                .checked_add(read as u64)
                .ok_or(ErrorCode::LimitExceeded)?;
            if count > limit {
                return Err(ErrorCode::LimitExceeded);
            }
            hash.update(&buffer[..read]);
        }
        Ok(hash.finish().as_ref() == expected)
    }
}
