//! Administrator recovery of durable wardrobe archives without overwriting later uploads.
use super::*;
use crate::service::library_policy::Actor;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecycleQuery {
    #[serde(default = "first_page")]
    pub page: u32,
    #[serde(default)]
    pub q: String,
    #[serde(default, rename = "type")]
    pub model_type: String,
    #[serde(default)]
    pub source: String,
}

fn first_page() -> u32 {
    1
}

impl Storage {
    pub(crate) fn recycle_preview(
        &self,
        actor: Actor,
        kind: &str,
        id: Uuid,
        owner: Option<Uuid>,
    ) -> Result<(AvatarDescriptor, PathBuf)> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if kind == "RECYCLE" {
            return self.wardrobe_recycle_source(
                actor,
                owner.ok_or(ErrorCode::InvalidMessage)?,
                id,
            );
        }
        if kind != "LIBRARY_RECYCLE" {
            return Err(ErrorCode::InvalidMessage);
        }
        let (owner, asset, revision): (String, String, i64) = self.library_read(|database| {
            database.query_row(
                "SELECT v.resource_owner, v.asset_id, v.resource_revision FROM library_models m
                 JOIN library_recycle r USING(model_id) JOIN library_revisions v ON v.model_id = m.model_id
                   AND v.revision = m.current_revision
                 WHERE m.model_id = ?1 AND m.moderation_status = 'DELETED' AND r.purged = 0",
                [id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(sqlite::sql_error)?.ok_or(ErrorCode::NotFound)
        })?;
        let owner = Uuid::parse_str(&owner).map_err(|_| ErrorCode::Internal)?;
        let avatar = self.avatar(
            owner,
            &asset,
            u64::try_from(revision).map_err(|_| ErrorCode::Internal)?,
        )?;
        let path = self
            .library_recycle_path(&id.to_string())?
            .join(faster_hex::hex_string(&avatar.raw_sha256));
        Ok((avatar, path))
    }

    pub(crate) fn wardrobe_purge(
        &self,
        actor: Actor,
        owner: Uuid,
        archive_id: Uuid,
        input: &Restore,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        let operation = crate::service::resource::operation_id(&input.operation_id)?;
        let fingerprint = sha256(
            &serde_json::to_vec(&("wardrobe-purge", owner, archive_id, input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        let _publication = self.lock_blob_files()?;
        let result = self.library_transaction(|transaction| {
            let bytes = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let (archive, _, _) = self.wardrobe_archive(owner, archive_id)?;
                if !Self::archive_deleted(transaction, &archive)? {
                    return Err(ErrorCode::NotFound);
                }
                let revision: i64 = transaction.query_row("SELECT revision FROM owners WHERE uuid = ?1",
                    [owner.to_string()], |row| row.get(0)).map_err(sqlite::sql_error)?;
                if revision != library::sql_integer(input.expected_equipment_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                transaction.execute(
                    "INSERT INTO library_audit(actor_uuid, action, model_id, created_at, result)
                     VALUES (?1, 'WARDROBE_PURGE', ?2, ?3, 'OK')",
                    params![actor.uuid.to_string(), archive_id.to_string(), library::sql_integer(now_seconds())?],
                ).map_err(sqlite::sql_error)?;
                serde_json::to_vec(&json!({"operation_id": input.operation_id, "purged": true, "asset_id": archive.asset_id}))
                    .map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice::<Value>(&bytes).map_err(|_| ErrorCode::Internal)
        })?;
        // Only this immutable archive is removed. Live/shared blobs remain reference protected.
        let folder = self
            .recycle
            .join("wardrobe")
            .join(owner.to_string())
            .join(archive_id.to_string());
        if folder.is_dir() {
            fs::remove_dir_all(folder).map_err(|_| ErrorCode::Internal)?;
        }
        self.clean_deleted_wardrobe(
            owner,
            result["asset_id"].as_str().ok_or(ErrorCode::Internal)?,
        )?;
        Ok(result)
    }

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
             SELECT 1 FROM library_audit WHERE action IN ('WARDROBE_RESTORE', 'WARDROBE_PURGE') AND model_id = ?4)",
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

    fn wardrobe_recycle_items(&self, actor: Actor, query: &RecycleQuery) -> Result<Vec<Value>> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if query.page == 0 || query.page > 1_000_000 || query.q.len() > 64 {
            return Err(ErrorCode::InvalidMessage);
        }
        if !matches!(
            query.model_type.as_str(),
            "" | "FIGURA" | "YSM_PLAIN" | "YSM_ENCRYPTED"
        ) || !matches!(query.source.as_str(), "" | "LIBRARY" | "WARDROBE")
        {
            return Err(ErrorCode::InvalidMessage);
        }
        if query.source == "LIBRARY" {
            return Ok(Vec::new());
        }
        let root = self.recycle.join("wardrobe");
        let owners = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
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
                let model_type = match avatar.model_type {
                    1 => "FIGURA",
                    2 => "YSM_PLAIN",
                    3 => "YSM_ENCRYPTED",
                    _ => return Err(ErrorCode::UnsupportedCapability),
                };
                if !query.model_type.is_empty() && query.model_type != model_type {
                    continue;
                }
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
                    Ok(Some(json!({"kind": "WARDROBE", "id": id, "owner": owner, "uploader_name": name,
                        "name": avatar.asset_id, "authors": [], "asset_id": avatar.asset_id,
                        "asset_revision": avatar.revision, "equipment_revision": revision, "model_type": model_type,
                        "operation_id": id, "deleted_at": archive.deleted_at, "policy_revision": 0,
                        "status": "DELETED", "downloads": 0,
                        "can_restore": !occupied && latest == library::sql_integer(archive.expected_revision)?})))
                })?;
                if let Some(item) = item {
                    items.push(item);
                }
            }
        }
        Ok(items)
    }

    pub(crate) fn library_recycle_list(&self, actor: Actor, query: &RecycleQuery) -> Result<Value> {
        let mut items = self.wardrobe_recycle_items(actor, query)?;
        self.library_read(|database| {
            let mut statement = database.prepare(
                "SELECT m.model_id, m.uploader_uuid, m.uploader_name_snapshot, m.display_name, m.authors_json,
                 m.policy_revision, m.current_revision, r.deleted_at, COALESCE(s.downloads, 0), m.model_type
                 FROM library_models m JOIN library_recycle r USING(model_id)
                 LEFT JOIN library_stats s USING(model_id)
                 WHERE m.published = 1 AND m.moderation_status = 'DELETED' AND r.purged = 0
                 AND (?2 = '' OR m.model_type = ?2) AND (?3 = '' OR ?3 = 'LIBRARY')
                 AND (?1 = '' OR instr(lower(m.display_name), lower(?1)) > 0
                   OR instr(lower(m.uploader_name_snapshot), lower(?1)) > 0 OR instr(m.uploader_uuid, lower(?1)) > 0)"
            ).map_err(sqlite::sql_error)?;
            let rows = statement.query_map(params![query.q, query.model_type, query.source], |row| {
                let authors: String = row.get(4)?;
                Ok(json!({"kind": "LIBRARY", "id": row.get::<_, String>(0)?,
                    "owner": row.get::<_, String>(1)?, "uploader_name": row.get::<_, String>(2)?,
                    "name": row.get::<_, String>(3)?, "authors": serde_json::from_str::<Value>(&authors).unwrap_or(json!([])),
                    "policy_revision": row.get::<_, i64>(5)?, "asset_revision": row.get::<_, i64>(6)?,
                    "deleted_at": row.get::<_, i64>(7)?, "downloads": row.get::<_, i64>(8)?,
                    "model_type": row.get::<_, String>(9)?, "status": "DELETED", "can_restore": true,
                    "equipment_revision": 0, "asset_id": ""}))
            }).map_err(sqlite::sql_error)?;
            for row in rows {
                items.push(row.map_err(sqlite::sql_error)?);
            }
            Ok(())
        })?;
        items.sort_by(|left, right| {
            right["deleted_at"]
                .as_u64()
                .cmp(&left["deleted_at"].as_u64())
                .then_with(|| {
                    left["kind"]
                        .as_str()
                        .cmp(&right["kind"].as_str())
                        .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
                })
        });
        let total = items.len();
        let start = (query.page as usize - 1) * 8;
        let mut page: Vec<_> = items.into_iter().skip(start).take(8).collect();
        for item in &mut page {
            if item["kind"] != "WARDROBE" {
                continue;
            }
            let owner = Uuid::parse_str(item["owner"].as_str().ok_or(ErrorCode::Internal)?)
                .map_err(|_| ErrorCode::Internal)?;
            let id = Uuid::parse_str(item["id"].as_str().ok_or(ErrorCode::Internal)?)
                .map_err(|_| ErrorCode::Internal)?;
            let (_, path) = self.wardrobe_recycle_source(actor, owner, id)?;
            if let Ok(identity) = crate::service::package::inspect(&path) {
                if let Some(name) = identity.internal_name {
                    item["name"] = json!(name);
                }
                item["authors"] = json!(identity.authors);
            }
        }
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
            let result = Ack::decode(result.as_slice()).map_err(|_| ErrorCode::Internal)?;
            self.finish_wardrobe_restore(owner, archive_id)?;
            return Ok(result);
        }
        let (archive, avatar, folder) = self.wardrobe_archive(owner, archive_id)?;
        if !self.library_read(|database| Self::archive_deleted(database, &archive))? {
            return Err(ErrorCode::NotFound);
        }
        let history = self
            .committed
            .lock()
            .map_err(|_| ErrorCode::Internal)?
            .owners
            .get(&owner)
            .and_then(|account| account.assets.get(&archive.asset_id))
            .cloned()
            .ok_or(ErrorCode::NotFound)?;
        let mut restored = Vec::new();
        for stored in history.values() {
            let descriptor = AvatarDescriptor::decode(stored.descriptor.as_slice())
                .map_err(|_| ErrorCode::Internal)?;
            let hash = faster_hex::hex_string(&descriptor.raw_sha256);
            let source = folder.join(&hash);
            if !source.is_file() && descriptor.revision != archive.expected_revision {
                continue;
            }
            let bytes = fs::read(&source).map_err(|_| ErrorCode::NotFound)?;
            if bytes.len() as u64 != descriptor.raw_length
                || sha256(&bytes) != descriptor.raw_sha256
            {
                return Err(ErrorCode::Internal);
            }
            recycle::copy_durable(&source, &self.wardrobe.join(hash))?;
            restored.push(descriptor);
        }
        let result = self.transaction_with_library_result(|manifest| {
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
            for descriptor in &restored {
                let stored = history.get_mut(&descriptor.revision).ok_or(ErrorCode::Conflict)?;
                if stored.descriptor != descriptor.encode_to_vec() {
                    return Err(ErrorCode::Conflict);
                }
                stored.tombstone = false;
            }
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
                "SELECT EXISTS(SELECT 1 FROM library_audit WHERE action IN ('WARDROBE_RESTORE', 'WARDROBE_PURGE') AND model_id = ?1)",
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
        })?;
        self.finish_wardrobe_restore(owner, archive_id)?;
        Ok(result)
    }

    fn finish_wardrobe_restore(&self, owner: Uuid, archive_id: Uuid) -> Result<()> {
        let folder = self
            .recycle
            .join("wardrobe")
            .join(owner.to_string())
            .join(archive_id.to_string());
        if folder.is_dir() {
            fs::remove_dir_all(folder).map_err(|_| ErrorCode::Internal)?;
        }
        Ok(())
    }
}
