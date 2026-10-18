//! Administrator recovery of durable wardrobe archives without overwriting later uploads.
use super::*;
use crate::service::library_policy::Actor;
use crate::storage::library_wardrobe::WardrobeQuery;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Archive {
    owner: Uuid,
    asset_id: String,
    expected_revision: u64,
    operation_id: Uuid,
    deleted_at: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Restore {
    pub operation_id: String,
    pub expected_equipment_revision: u64,
}

impl Storage {
    fn wardrobe_archive(
        &self,
        owner: Uuid,
        operation: Uuid,
    ) -> Result<(Archive, AvatarDescriptor, PathBuf)> {
        let folder = self
            .recycle
            .join("wardrobe")
            .join(owner.to_string())
            .join(operation.to_string());
        let bytes = fs::read(folder.join("metadata.json")).map_err(|_| ErrorCode::NotFound)?;
        let archive: Archive = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)?;
        if archive.owner != owner
            || archive.operation_id != operation
            || archive.expected_revision == 0
        {
            return Err(ErrorCode::Internal);
        }
        let bytes = fs::read(folder.join(format!("revision-{}.pb", archive.expected_revision)))
            .map_err(|_| ErrorCode::NotFound)?;
        let avatar = AvatarDescriptor::decode(bytes.as_slice()).map_err(|_| ErrorCode::Internal)?;
        if avatar.owner != owner.to_string()
            || avatar.asset_id != archive.asset_id
            || avatar.revision != archive.expected_revision
            || avatar.raw_sha256.len() != 32
        {
            return Err(ErrorCode::Internal);
        }
        Ok((archive, avatar, folder))
    }

    fn archive_deleted(database: &rusqlite::Connection, archive: &Archive) -> Result<bool> {
        database
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM avatars WHERE owner_uuid = ?1 AND asset_id = ?2
             AND revision = ?3 AND tombstone = 1) AND NOT EXISTS(
             SELECT 1 FROM library_audit WHERE action = 'WARDROBE_RESTORE' AND model_id = ?4)",
                params![
                    archive.owner.to_string(),
                    archive.asset_id,
                    library::sql_integer(archive.expected_revision)?,
                    archive.operation_id.to_string()
                ],
                |row| row.get(0),
            )
            .map_err(sqlite::sql_error)
    }

    pub(crate) fn wardrobe_recycle_source(
        &self,
        actor: Actor,
        owner: Uuid,
        operation: Uuid,
    ) -> Result<(AvatarDescriptor, PathBuf)> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        let (archive, avatar, folder) = self.wardrobe_archive(owner, operation)?;
        if !self.library_read(|database| Self::archive_deleted(database, &archive))? {
            return Err(ErrorCode::NotFound);
        }
        let path = folder.join(faster_hex::hex_string(&avatar.raw_sha256));
        Ok((avatar, path))
    }

    pub(crate) fn wardrobe_recycle_list(
        &self,
        actor: Actor,
        query: &WardrobeQuery,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if query.page == 0 || query.page > 1_000_000 || query.q.len() > 64 {
            return Err(ErrorCode::InvalidMessage);
        }
        let root = self.recycle.join("wardrobe");
        let owners = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(
                    json!({"items": [], "page": query.page, "page_size": 8, "total": 0, "has_next": false}),
                );
            }
            Err(_) => return Err(ErrorCode::Internal),
        };
        let mut items = Vec::new();
        for entry in owners {
            let entry = entry.map_err(|_| ErrorCode::Internal)?;
            if !entry.file_type().map_err(|_| ErrorCode::Internal)?.is_dir() {
                continue;
            }
            let Ok(owner) = Uuid::parse_str(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            for operation in fs::read_dir(entry.path()).map_err(|_| ErrorCode::Internal)? {
                let operation = operation.map_err(|_| ErrorCode::Internal)?;
                if !operation
                    .file_type()
                    .map_err(|_| ErrorCode::Internal)?
                    .is_dir()
                {
                    continue;
                }
                let Ok(id) = Uuid::parse_str(&operation.file_name().to_string_lossy()) else {
                    continue;
                };
                let (archive, avatar, _) = self.wardrobe_archive(owner, id)?;
                let item = self.library_read(|database| {
                    if !Self::archive_deleted(database, &archive)? {
                        return Ok(None);
                    }
                    let (revision, name, occupied, latest): (i64, String, bool, i64) = database.query_row(
                        "SELECT o.revision, COALESCE(json_extract(u.profile, '$.nickname'), ''),
                         EXISTS(SELECT 1 FROM equipment e WHERE e.owner_uuid = o.uuid),
                         (SELECT MAX(a.revision) FROM avatars a WHERE a.owner_uuid = o.uuid AND a.asset_id = ?2)
                         FROM owners o LEFT JOIN users u ON u.uuid = o.uuid WHERE o.uuid = ?1",
                        params![owner.to_string(), archive.asset_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    ).map_err(sqlite::sql_error)?;
                    let query = query.q.to_lowercase();
                    if !query.is_empty() && !name.to_lowercase().contains(&query) && !owner.to_string().contains(&query) {
                        return Ok(None);
                    }
                    Ok(Some(json!({"owner": owner, "name": name, "asset_id": avatar.asset_id,
                        "asset_revision": avatar.revision, "equipment_revision": revision, "model_type": avatar.model_type,
                        "operation_id": id, "deleted_at": archive.deleted_at,
                        "can_restore": !occupied && latest == library::sql_integer(archive.expected_revision)?})))
                })?;
                if let Some(item) = item {
                    items.push(item);
                }
            }
        }
        items.sort_by(|left, right| {
            right["deleted_at"]
                .as_u64()
                .cmp(&left["deleted_at"].as_u64())
                .then_with(|| {
                    left["operation_id"]
                        .as_str()
                        .cmp(&right["operation_id"].as_str())
                })
        });
        let total = items.len();
        let start = (query.page as usize - 1) * 8;
        let page: Vec<_> = items.into_iter().skip(start).take(8).collect();
        Ok(
            json!({"items": page, "page": query.page, "page_size": 8, "total": total,
            "has_next": (query.page as usize) * 8 < total}),
        )
    }

    pub(crate) fn wardrobe_restore(
        &self,
        actor: Actor,
        owner: Uuid,
        archive_id: Uuid,
        input: &Restore,
    ) -> Result<Ack> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        let operation = crate::service::resource::operation_id(&input.operation_id)?;
        let fingerprint = sha256(
            &serde_json::to_vec(&("wardrobe-restore", owner, archive_id, input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        let _publication = self.lock_blob_files()?;
        // Durable replay must work after the archive leaves the list, including after restart.
        let prior = self.library_read(|database| database.query_row(
            "SELECT fingerprint, result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
            params![actor.uuid.to_string(), input.operation_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        ).optional().map_err(sqlite::sql_error))?;
        if let Some((saved, result)) = prior {
            if saved != fingerprint {
                return Err(ErrorCode::Conflict);
            }
            return Ack::decode(result.as_slice()).map_err(|_| ErrorCode::Internal);
        }
        let (archive, avatar, folder) = self.wardrobe_archive(owner, archive_id)?;
        if !self.library_read(|database| Self::archive_deleted(database, &archive))? {
            return Err(ErrorCode::NotFound);
        }
        let source = folder.join(faster_hex::hex_string(&avatar.raw_sha256));
        let bytes = fs::read(&source).map_err(|_| ErrorCode::NotFound)?;
        if bytes.len() as u64 != avatar.raw_length || sha256(&bytes) != avatar.raw_sha256 {
            return Err(ErrorCode::Internal);
        }
        let destination = self
            .wardrobe
            .join(faster_hex::hex_string(&avatar.raw_sha256));
        recycle::copy_durable(&source, &destination)?;
        self.transaction_with_library_result(|manifest| {
            if let Some(result) = replay::<Ack>(manifest, actor.uuid, &input.operation_id, &fingerprint)? {
                return Ok(result);
            }
            let account = manifest.owners.get_mut(&owner).ok_or(ErrorCode::NotFound)?;
            let history = account.assets.get_mut(&archive.asset_id).ok_or(ErrorCode::NotFound)?;
            let latest_revision = history.last_key_value().map(|(revision, _)| *revision);
            if account.revision != input.expected_equipment_revision
                || account.primary.is_some()
                || account.compatibility.is_some()
                || latest_revision != Some(archive.expected_revision)
                || history.values().any(|stored| !stored.tombstone)
            {
                return Err(ErrorCode::Conflict);
            }
            let stored = history.get_mut(&archive.expected_revision).ok_or(ErrorCode::NotFound)?;
            if stored.descriptor != avatar.encode_to_vec() {
                return Err(ErrorCode::Conflict);
            }
            stored.tombstone = false;
            let reference = Some(asset_ref(&avatar));
            let compatibility = if avatar.model_type == 1 {
                reference.clone()
            } else {
                None
            };
            select(account, reference, compatibility);
            let result = Ack {
                operation_id: input.operation_id.clone(),
                revision: account.revision,
                sequence: 0,
            };
            remember(manifest, actor.uuid, &input.operation_id, &fingerprint, &result)?;
            Ok(result)
        }, |transaction, result| {
            let restored: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_audit WHERE action = 'WARDROBE_RESTORE' AND model_id = ?1)",
                [archive_id.to_string()], |row| row.get(0),
            ).map_err(sqlite::sql_error)?;
            if restored {
                return Err(ErrorCode::Conflict);
            }
            library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                transaction.execute(
                    "INSERT INTO library_audit(actor_uuid, action, model_id, created_at, result)
                     VALUES (?1, 'WARDROBE_RESTORE', ?2, ?3, 'OK')",
                    params![actor.uuid.to_string(), archive_id.to_string(), library::sql_integer(now_seconds())?],
                ).map_err(sqlite::sql_error)?;
                Ok(result.encode_to_vec())
            })?;
            Ok(())
        })
    }
}
