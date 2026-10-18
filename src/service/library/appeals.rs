use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppealInput {
    pub operation_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveAppeal {
    pub operation_id: String,
    pub expected_revision: u64,
    pub response: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppealQuery {
    #[serde(default = "first_page")]
    pub page: u32,
    #[serde(default)]
    pub resolved: bool,
}

fn first_page() -> u32 {
    1
}

/// A restricted exception for the uploader's appeal page, never an ordinary model detail.
fn owned_target(database: &Connection, actor: Uuid, kind: &str, id: &str) -> Result<i64> {
    super::super::resource::operation_id(id)?;
    let allowed: bool = match kind {
        "LIBRARY" => database.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_effective_models WHERE model_id = ?1
             AND uploader_uuid = ?2 AND moderation_status = 'BLOCKED' AND published = 1)",
            params![id, actor.to_string()],
            |row| row.get(0),
        ),
        "WARDROBE" => database.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_wardrobe_blocks WHERE operation_id = ?1
             AND owner_uuid = ?2 AND active = 1)",
            params![id, actor.to_string()],
            |row| row.get(0),
        ),
        _ => return Err(ErrorCode::InvalidMessage),
    }
    .map_err(sql_error)?;
    if !allowed {
        // A wardrobe uploader can appeal a ban originating from another catalog record or copy,
        // but receives only the sanction identifier and their own asset name.
        let source = match kind {
            "LIBRARY" => "library_blocked_hashes",
            "WARDROBE" => "library_wardrobe_blocks",
            _ => return Err(ErrorCode::InvalidMessage),
        };
        let identifier = if kind == "LIBRARY" {
            "blocking_model_id"
        } else {
            "operation_id"
        };
        let owns_copy: bool = database.query_row(&format!(
            "SELECT EXISTS(SELECT 1 FROM equipment e JOIN avatars a
             ON a.owner_uuid = e.owner_uuid AND a.asset_id = e.asset_id AND a.revision = e.revision
             JOIN library_verified_blobs v ON v.raw_sha256 = lower(hex(a.raw_sha256))
             JOIN {source} b ON b.hash_kind = v.hash_kind AND b.verified_hash = v.verified_hash AND b.active = 1
             WHERE e.owner_uuid = ?1 AND e.slot = 'primary' AND b.{identifier} = ?2
               AND NOT EXISTS(SELECT 1 FROM library_resource_bindings r WHERE r.resource_owner = e.owner_uuid
                 AND r.asset_id = e.asset_id AND r.revision = e.revision))"
        ), params![actor.to_string(), id], |row| row.get(0)).map_err(sql_error)?;
        if !owns_copy {
            return Err(ErrorCode::NotFound);
        }
    }
    if kind == "LIBRARY" {
        database
            .query_row(
                "SELECT policy_revision FROM library_models WHERE model_id = ?1",
                [id],
                |row| row.get(0),
            )
            .map_err(sql_error)
    } else {
        Ok(1)
    }
}

impl Storage {
    pub fn library_appeal_targets(&self, actor: Actor, page: u32) -> Result<Value> {
        if page == 0 || page > 1_000_000 {
            return Err(ErrorCode::InvalidMessage);
        }
        if let Some(avatar) = self.equipped(actor.uuid)?.primary {
            if !self.library_asset_bound(actor.uuid, &avatar.asset_id)? {
                self.library_ensure_verified_blob(&avatar)?;
            }
        }
        self.library_read(|database| {
            let sources = "SELECT 'LIBRARY' AS target_kind, model_id AS target_id, display_name AS name
                FROM library_effective_models WHERE uploader_uuid = ?1 AND moderation_status = 'BLOCKED' AND published = 1
                UNION ALL SELECT 'WARDROBE', operation_id, asset_id FROM library_wardrobe_blocks
                WHERE owner_uuid = ?1 AND active = 1 GROUP BY operation_id, asset_id
                UNION ALL SELECT 'LIBRARY', b.blocking_model_id, e.asset_id FROM equipment e JOIN avatars a
                ON a.owner_uuid = e.owner_uuid AND a.asset_id = e.asset_id AND a.revision = e.revision
                JOIN library_verified_blobs v ON v.raw_sha256 = lower(hex(a.raw_sha256))
                JOIN library_blocked_hashes b ON b.hash_kind = v.hash_kind AND b.verified_hash = v.verified_hash AND b.active = 1
                WHERE e.owner_uuid = ?1 AND e.slot = 'primary' AND NOT EXISTS(SELECT 1 FROM library_resource_bindings r
                  WHERE r.resource_owner = e.owner_uuid AND r.asset_id = e.asset_id AND r.revision = e.revision)
                UNION ALL SELECT 'WARDROBE', b.operation_id, e.asset_id FROM equipment e JOIN avatars a
                ON a.owner_uuid = e.owner_uuid AND a.asset_id = e.asset_id AND a.revision = e.revision
                JOIN library_verified_blobs v ON v.raw_sha256 = lower(hex(a.raw_sha256))
                JOIN library_wardrobe_blocks b ON b.hash_kind = v.hash_kind AND b.verified_hash = v.verified_hash AND b.active = 1
                WHERE e.owner_uuid = ?1 AND e.slot = 'primary' AND NOT EXISTS(SELECT 1 FROM library_resource_bindings r
                  WHERE r.resource_owner = e.owner_uuid AND r.asset_id = e.asset_id AND r.revision = e.revision)";
            let targets = format!("SELECT target_kind, target_id, min(name) AS name FROM ({sources}) GROUP BY target_kind, target_id");
            let total: i64 = database.query_row(&format!("SELECT count(*) FROM ({targets})"),
                [actor.uuid.to_string()], |row| row.get(0)).map_err(sql_error)?;
            let mut statement = database.prepare(&format!("SELECT * FROM ({targets}) ORDER BY target_kind, target_id LIMIT 8 OFFSET ?2"))
                .map_err(sql_error)?;
            let rows = statement.query_map(params![actor.uuid.to_string(), i64::from(page - 1) * 8], |row| {
                Ok(json!({"target_kind": row.get::<_, String>(0)?, "target_id": row.get::<_, String>(1)?, "name": row.get::<_, String>(2)?}))
            }).map_err(sql_error)?;
            let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>().map_err(sql_error)?;
            Ok(json!({"items": items, "total": total, "page": page, "page_size": 8, "has_next": i64::from(page)*8 < total}))
        })
    }

    pub fn library_appeal(&self, actor: Actor, input: &AppealInput) -> Result<Value> {
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        text(input.reason.trim(), 512, false)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!([
                "appeal",
                input.target_kind,
                input.target_id,
                input.reason.trim(),
            ]))
            .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let target_revision = owned_target(transaction, actor.uuid, &input.target_kind, &input.target_id)?;
                let exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_appeals WHERE uploader_uuid = ?1
                     AND target_kind = ?2 AND target_id = ?3 AND resolved_at IS NULL)",
                    params![actor.uuid.to_string(), input.target_kind, input.target_id], |row| row.get(0),
                ).map_err(sql_error)?;
                if exists { return Err(ErrorCode::Conflict); }
                let appeal = random_id();
                transaction.execute(
                    "INSERT INTO library_appeals(appeal_id, target_kind, target_id, uploader_uuid, reason, created_at, target_revision)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![appeal, input.target_kind, input.target_id, actor.uuid.to_string(), input.reason.trim(), library::sql_integer(now_seconds())?, target_revision],
                ).map_err(sql_error)?;
                serde_json::to_vec(&json!({"appeal_id": appeal, "submitted": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_appeals(&self, actor: Actor, query: &AppealQuery, admin: bool) -> Result<Value> {
        if admin && !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        if query.page == 0 || query.page > 1_000_000 {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_read(|database| {
            let total: i64 = database.query_row(
                "SELECT count(*) FROM library_appeals WHERE (?1 OR uploader_uuid = ?2) AND (resolved_at IS NOT NULL) = ?3",
                params![admin, actor.uuid.to_string(), query.resolved], |row| row.get(0),
            ).map_err(sql_error)?;
            let mut statement = database.prepare(
                "SELECT appeal_id, target_kind, target_id, uploader_uuid, reason, created_at, revision, resolved_at,
                 resolved_by, response FROM library_appeals WHERE (?1 OR uploader_uuid = ?2)
                 AND (resolved_at IS NOT NULL) = ?3 ORDER BY created_at, appeal_id LIMIT 8 OFFSET ?4",
            ).map_err(sql_error)?;
            let rows = statement.query_map(params![admin, actor.uuid.to_string(), query.resolved, i64::from(query.page - 1) * 8], |row| {
                Ok(json!({"appeal_id": row.get::<_, String>(0)?, "target_kind": row.get::<_, String>(1)?,
                    "target_id": row.get::<_, String>(2)?, "uploader_uuid": row.get::<_, String>(3)?,
                    "reason": row.get::<_, String>(4)?, "created_at": row.get::<_, i64>(5)?, "revision": row.get::<_, i64>(6)?,
                    "resolved_at": row.get::<_, Option<i64>>(7)?, "resolved_by": if admin { row.get::<_, Option<String>>(8)? } else { None },
                    "response": row.get::<_, String>(9)?}))
            }).map_err(sql_error)?;
            let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>().map_err(sql_error)?;
            Ok(json!({"items": items, "total": total, "page": query.page, "page_size": 8, "has_next": i64::from(query.page)*8 < total}))
        })
    }

    pub fn library_resolve_appeal(
        &self,
        actor: Actor,
        id: &str,
        input: &ResolveAppeal,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        super::super::resource::operation_id(id)?;
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        text(input.response.trim(), 512, false)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!([
                "resolve_appeal",
                id,
                input.expected_revision,
                input.response.trim(),
            ]))
            .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let changed = transaction.execute(
                    "UPDATE library_appeals SET resolved_at = ?1, resolved_by = ?2, response = ?3, revision = revision + 1
                     WHERE appeal_id = ?4 AND revision = ?5 AND resolved_at IS NULL",
                    params![library::sql_integer(now_seconds())?, actor.uuid.to_string(), input.response.trim(), id,
                        library::sql_integer(input.expected_revision)?],
                ).map_err(sql_error)?;
                if changed != 1 { return Err(ErrorCode::Conflict); }
                serde_json::to_vec(&json!({"appeal_id": id, "resolved": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }

    pub fn library_unblock_appeal(
        &self,
        actor: Actor,
        id: &str,
        input: &Mutation,
    ) -> Result<Value> {
        if !actor.admin_context {
            return Err(ErrorCode::Forbidden);
        }
        super::super::resource::operation_id(id)?;
        let operation = super::super::resource::operation_id(&input.operation_id)?;
        let fingerprint = crate::storage::sha256(
            &serde_json::to_vec(&json!(["unblock_appeal", id, input.expected_revision,]))
                .map_err(|_| ErrorCode::Internal)?,
        );
        self.library_transaction(|transaction| {
            let result = library::operation(transaction, actor.uuid, operation, &fingerprint, now_seconds(), |transaction| {
                let target: Option<(String, String, i64)> = transaction.query_row(
                    "SELECT target_kind, target_id, target_revision FROM library_appeals
                     WHERE appeal_id = ?1 AND revision = ?2 AND resolved_at IS NULL",
                    params![id, library::sql_integer(input.expected_revision)?],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                ).optional().map_err(sql_error)?;
                let (kind, target_id, target_revision) = target.ok_or(ErrorCode::Conflict)?;
                if kind == "LIBRARY" {
                    library::set_blocked(transaction, &target_id, actor.uuid,
                        u64::try_from(target_revision).map_err(|_| ErrorCode::Internal)?, false,
                        "Appeal review", now_seconds())?;
                } else {
                    let changed = transaction.execute(
                        "UPDATE library_wardrobe_blocks SET active = 0 WHERE operation_id = ?1 AND active = 1", [&target_id],
                    ).map_err(sql_error)?;
                    if changed == 0 { return Err(ErrorCode::Conflict); }
                    // Removing a wardrobe ban never republishes affected library copies.
                    transaction.execute(
                        "UPDATE library_models SET visibility = 'PRIVATE', policy_revision = policy_revision + 1,
                         updated_at = ?1 WHERE moderation_status != 'DELETED' AND model_id IN (
                           SELECT r.model_id FROM library_revision_hashes r JOIN library_wardrobe_blocks b
                           ON r.hash_kind = b.hash_kind AND r.verified_hash = b.verified_hash WHERE b.operation_id = ?2)",
                        params![library::sql_integer(now_seconds())?, target_id],
                    ).map_err(sql_error)?;
                    transaction.execute(
                        "INSERT INTO library_audit(actor_uuid, action, model_id, created_at, result)
                         VALUES (?1, 'WARDROBE_UNBLOCK', ?2, ?3, 'OK')",
                        params![actor.uuid.to_string(), target_id, library::sql_integer(now_seconds())?],
                    ).map_err(sql_error)?;
                }
                transaction.execute("UPDATE library_appeals SET revision = revision + 1 WHERE appeal_id = ?1", [id])
                    .map_err(sql_error)?;
                serde_json::to_vec(&json!({"appeal_id": id, "unblocked": true})).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&result).map_err(|_| ErrorCode::Internal)
        })
    }
}
