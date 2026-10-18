//! Explicit administrator actions on a player's current, non-library wardrobe resource.
use super::*;
use crate::service::library_policy::Actor;
use rusqlite::params;
use serde_json::{json, Value};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WardrobeAction {
    pub operation_id: String,
    pub expected_equipment_revision: u64,
    pub asset_id: String,
    pub asset_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WardrobeQuery {
    #[serde(default = "first_page")]
    pub page: u32,
    #[serde(default)]
    pub q: String,
}

fn first_page() -> u32 {
    1
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WardrobeVisibility {
    pub operation_id: String,
    pub expected_equipment_revision: u64,
    pub asset_id: String,
    pub asset_revision: u64,
    pub show_equipment: bool,
}

fn banned(
    database: &rusqlite::Connection,
    owner: Uuid,
    asset: &str,
    revision: u64,
) -> Result<bool> {
    database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM avatars a JOIN library_verified_blobs v
         ON v.raw_sha256 = lower(hex(a.raw_sha256)) JOIN library_active_blocked_hashes b
         ON b.hash_kind = v.hash_kind AND b.verified_hash = v.verified_hash AND b.active = 1
         WHERE a.owner_uuid = ?1 AND a.asset_id = ?2 AND a.revision = ?3)",
            params![owner.to_string(), asset, library::sql_integer(revision)?],
            |row| row.get(0),
        )
        .map_err(sqlite::sql_error)
}

impl Storage {
    pub(crate) fn library_wardrobe_self(&self, owner: Uuid) -> Result<Value> {
        let equipped = self.equipped(owner)?;
        let Some(avatar) = equipped.primary else {
            return Ok(json!({"available": false}));
        };
        if self.library_asset_bound(owner, &avatar.asset_id)? {
            return Ok(json!({"available": false}));
        }
        self.library_ensure_verified_blob(&avatar)?;
        self.library_read(|database| {
            let blocked = banned(database, owner, &avatar.asset_id, avatar.revision)?;
            let hidden: bool = database.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_wardrobe_settings WHERE owner_uuid = ?1
                 AND asset_id = ?2 AND asset_revision = ?3 AND show_equipment = 0)",
                params![owner.to_string(), avatar.asset_id, library::sql_integer(avatar.revision)?], |row| row.get(0),
            ).map_err(sqlite::sql_error)?;
            Ok(json!({"available": true, "asset_id": avatar.asset_id, "asset_revision": avatar.revision,
                "equipment_revision": equipped.revision, "show_equipment": !blocked && !hidden, "blocked": blocked}))
        })
    }

    pub(crate) fn library_wardrobe_visibility(
        &self,
        owner: Uuid,
        input: &WardrobeVisibility,
    ) -> Result<Ack> {
        let operation = crate::service::resource::operation_id(&input.operation_id)?;
        if input.asset_id.len() > 128 {
            return Err(ErrorCode::InvalidMessage);
        }
        let fingerprint = sha256(
            &serde_json::to_vec(&("wardrobe-visibility", owner, input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        let avatar = self.avatar(owner, &input.asset_id, input.asset_revision)?;
        self.library_ensure_verified_blob(&avatar)?;
        self.transaction_with_library_result(|manifest| {
            if let Some(result) = replay::<Ack>(manifest, owner, &input.operation_id, &fingerprint)? {
                return Ok(result);
            }
            let account = manifest.owners.get_mut(&owner).ok_or(ErrorCode::NotFound)?;
            if account.revision != input.expected_equipment_revision
                || account.primary.as_ref().is_none_or(|reference|
                    reference.id != input.asset_id || reference.revision != input.asset_revision) {
                return Err(ErrorCode::Conflict);
            }
            account.revision = account.revision.checked_add(1).ok_or(ErrorCode::Internal)?;
            let result = Ack { operation_id: input.operation_id.clone(), revision: account.revision, sequence: 0 };
            remember(manifest, owner, &input.operation_id, &fingerprint, &result)?;
            Ok(result)
        }, |transaction, result| {
            let bound: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1 AND asset_id = ?2 AND revision = ?3)",
                params![owner.to_string(), input.asset_id, library::sql_integer(input.asset_revision)?], |row| row.get(0),
            ).map_err(sqlite::sql_error)?;
            if bound || input.show_equipment && banned(transaction, owner, &input.asset_id, input.asset_revision)? {
                return Err(ErrorCode::Forbidden);
            }
            library::operation(transaction, owner, operation, &fingerprint, now_seconds(), |transaction| {
                transaction.execute(
                    "INSERT INTO library_wardrobe_settings VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(owner_uuid, asset_id, asset_revision) DO UPDATE SET show_equipment = excluded.show_equipment",
                    params![owner.to_string(), input.asset_id, library::sql_integer(input.asset_revision)?, input.show_equipment],
                ).map_err(sqlite::sql_error)?;
                Ok(result.encode_to_vec())
            })?;
            Ok(())
        })
    }

    pub(crate) fn library_wardrobe_list(
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
        self.library_read(|database| {
            // Query only the current primary, never enumerate uploaded history or library aliases.
            let predicate = "FROM equipment e JOIN owners o ON o.uuid = e.owner_uuid
                LEFT JOIN users u ON u.uuid = e.owner_uuid
                JOIN avatars a ON a.owner_uuid = e.owner_uuid AND a.asset_id = e.asset_id AND a.revision = e.revision
                WHERE e.slot = 'primary' AND a.tombstone = 0
                AND NOT EXISTS(SELECT 1 FROM library_resource_bindings b WHERE b.resource_owner = e.owner_uuid
                    AND b.asset_id = e.asset_id AND b.revision = e.revision)
                AND (?1 = '' OR instr(lower(COALESCE(json_extract(u.profile, '$.nickname'), '')), lower(?1)) > 0
                    OR instr(e.owner_uuid, lower(?1)) > 0)";
            let total: i64 = database.query_row(&format!("SELECT count(*) {predicate}"), [&query.q], |row| row.get(0))
                .map_err(sqlite::sql_error)?;
            let mut statement = database.prepare(&format!(
                "SELECT e.owner_uuid, COALESCE(json_extract(u.profile, '$.nickname'), ''),
                 e.asset_id, e.revision, o.revision, a.model_type {predicate} ORDER BY e.owner_uuid LIMIT 8 OFFSET ?2"
            )).map_err(sqlite::sql_error)?;
            let rows = statement.query_map(params![query.q, i64::from(query.page - 1) * 8], |row| {
                Ok(json!({"owner": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?,
                    "asset_id": row.get::<_, String>(2)?, "asset_revision": row.get::<_, i64>(3)?,
                    "equipment_revision": row.get::<_, i64>(4)?, "model_type": row.get::<_, i32>(5)?}))
            }).map_err(sqlite::sql_error)?;
            let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>().map_err(sqlite::sql_error)?;
            Ok(json!({"items": items, "total": total, "page": query.page, "page_size": 8,
                "has_next": i64::from(query.page) * 8 < total}))
        })
    }

    pub(crate) fn library_wardrobe_action(
        &self,
        actor: Actor,
        owner: Uuid,
        input: &WardrobeAction,
        block: bool,
    ) -> Result<Ack> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        let operation = crate::service::resource::operation_id(&input.operation_id)?;
        if input.asset_id.is_empty()
            || input.asset_id.len() > 128
            || !input
                .asset_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let fingerprint = sha256(
            &serde_json::to_vec(&("admin-wardrobe", owner, block, input))
                .map_err(|_| ErrorCode::Internal)?,
        );
        let prior = self.library_read(|database| {
            use rusqlite::OptionalExtension;
            database.query_row(
                "SELECT fingerprint, result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
                params![actor.uuid.to_string(), input.operation_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            ).optional().map_err(sqlite::sql_error)
        })?;
        if let Some((saved, result)) = prior {
            if saved != fingerprint {
                return Err(ErrorCode::Conflict);
            }
            return Ack::decode(result.as_slice()).map_err(|_| ErrorCode::Internal);
        }
        // Resolve immutable bytes before taking the manifest transaction lock.
        let avatar = self.avatar(owner, &input.asset_id, input.asset_revision)?;
        if block {
            self.library_ensure_verified_blob(&avatar)?;
        }
        let raw_hash = faster_hex::hex_string(&avatar.raw_sha256);
        self.transaction_with_library_result(|manifest| {
            if let Some(result) = replay::<Ack>(manifest, actor.uuid, &input.operation_id, &fingerprint)? {
                return Ok(result);
            }
            let account = manifest.owners.get_mut(&owner).ok_or(ErrorCode::NotFound)?;
            if account.revision != input.expected_equipment_revision
                || account.primary.as_ref().is_none_or(|reference|
                    reference.id != input.asset_id || reference.revision != input.asset_revision) {
                return Err(ErrorCode::Conflict);
            }
            if !block {
                select(account, None, None);
            }
            let result = Ack { operation_id: input.operation_id.clone(), revision: account.revision, sequence: 0 };
            remember(manifest, actor.uuid, &input.operation_id, &fingerprint, &result)?;
            Ok(result)
        }, |transaction, result| {
            let bound: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1
                 AND asset_id = ?2 AND revision = ?3)",
                params![owner.to_string(), input.asset_id, library::sql_integer(input.asset_revision)?], |row| row.get(0),
            ).map_err(sqlite::sql_error)?;
            if bound {
                return Err(ErrorCode::Forbidden);
            }
            let saved = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                if block {
                    transaction.execute(
                        "INSERT INTO library_wardrobe_blocks(operation_id, hash_kind, verified_hash, owner_uuid,
                         asset_id, asset_revision, actor_uuid, created_at)
                         SELECT ?1, hash_kind, verified_hash, ?2, ?3, ?4, ?5, ?6
                         FROM library_verified_blobs WHERE raw_sha256 = ?7",
                        params![input.operation_id, owner.to_string(), input.asset_id, library::sql_integer(input.asset_revision)?,
                            actor.uuid.to_string(), library::sql_integer(now_seconds())?, raw_hash],
                    ).map_err(sqlite::sql_error)?;
                    transaction.execute(
                        "UPDATE library_models SET visibility = 'PRIVATE', policy_revision = policy_revision + 1,
                         updated_at = ?1 WHERE moderation_status != 'DELETED' AND model_id IN (
                           SELECT r.model_id FROM library_revision_hashes r JOIN library_verified_blobs v
                           ON r.hash_kind = v.hash_kind AND r.verified_hash = v.verified_hash WHERE v.raw_sha256 = ?2)",
                        params![library::sql_integer(now_seconds())?, raw_hash],
                    ).map_err(sqlite::sql_error)?;
                }
                transaction.execute(
                    "INSERT INTO library_audit(actor_uuid, action, model_id, created_at, result) VALUES (?1, ?2, ?3, ?4, 'OK')",
                    params![actor.uuid.to_string(), if block { "WARDROBE_BLOCK" } else { "WARDROBE_RESET" },
                        owner.to_string(), library::sql_integer(now_seconds())?],
                ).map_err(sqlite::sql_error)?;
                Ok(result.encode_to_vec())
            })?;
            if saved != result.encode_to_vec() {
                return Err(ErrorCode::Conflict);
            }
            Ok(())
        })
    }
}
