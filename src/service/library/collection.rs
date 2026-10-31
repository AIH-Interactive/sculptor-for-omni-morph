//! Account membership is independent of permissions, favorites and equipped resources.
use super::*;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionQuery {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinInput {
    pub transfer_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutfitInput {
    pub in_outfit: bool,
}

impl Storage {
    /// Forget only the authenticated player's membership, including unavailable models.
    pub fn library_collection_remove(&self, actor: Actor, id: &str) -> Result<Value> {
        super::super::resource::operation_id(id)?;
        self.library_transaction(|transaction| {
            transaction
                .execute(
                    "DELETE FROM library_collection WHERE player_uuid = ?1 AND model_id = ?2",
                    params![actor.uuid.to_string(), id],
                )
                .map_err(sql_error)?;
            Ok(json!({"model_id": id, "joined": false}))
        })
    }

    pub fn library_collection(&self, actor: Actor, query: &CollectionQuery) -> Result<Value> {
        let page = query.page.unwrap_or(1);
        let size = query.page_size.unwrap_or(32);
        if page == 0 || !(1..=64).contains(&size) {
            return Err(ErrorCode::InvalidMessage);
        }
        self.library_read(|database| {
            let player = actor.uuid.to_string();
            let total: i64 = database
                .query_row(
                    "SELECT count(*) FROM library_collection WHERE player_uuid = ?1",
                    [&player],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let offset = i64::from(page - 1) * i64::from(size);
            let mut statement = database
                .prepare(
                    "SELECT model_id, joined_at, join_source, in_outfit FROM library_collection
                 WHERE player_uuid = ?1 ORDER BY joined_at DESC, model_id LIMIT ?2 OFFSET ?3",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![player, size, offset], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                })
                .map_err(sql_error)?;
            let mut items = Vec::new();
            for row in rows {
                let (id, joined_at, source, in_outfit) = row.map_err(sql_error)?;
                // A stale member never discloses metadata hidden by current policy.
                let summary = match model(database, &id, actor, false) {
                    Ok(item) => Some(item.summary),
                    Err(ErrorCode::NotFound | ErrorCode::Forbidden) => None,
                    Err(error) => return Err(error),
                };
                items.push(json!({"model_id": id, "joined_at": joined_at,
                    "join_source": source, "in_outfit": in_outfit, "model": summary}));
            }
            Ok(json!({"items": items, "page": page, "page_size": size,
                "total": total, "has_next": offset + i64::from(size) < total}))
        })
    }

    pub fn library_collection_join(
        &self,
        actor: Actor,
        id: &str,
        input: &JoinInput,
    ) -> Result<Value> {
        super::super::resource::operation_id(&input.transfer_id)?;
        self.library_transaction(|transaction| {
            let item = model(transaction, id, actor, false)?;
            require_action(&item, "download")?;
            let eligible: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_download_receipts
                 WHERE transfer_id = ?1 AND player_uuid = ?2 AND model_id = ?3
                 AND revision = ?4 AND policy_revision = ?5 AND purpose = 'download'
                 AND completed = 1 AND expires_at > ?6)",
                    params![
                        input.transfer_id,
                        actor.uuid.to_string(),
                        id,
                        item.summary.revision,
                        item.summary.policy_revision,
                        library::sql_integer(now_seconds())?
                    ],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if !eligible {
                return Err(ErrorCode::Conflict);
            }
            transaction
                .execute(
                    "INSERT OR IGNORE INTO library_collection
                 (player_uuid, model_id, joined_at, join_source) VALUES (?1, ?2, ?3, 'DOWNLOAD')",
                    params![
                        actor.uuid.to_string(),
                        id,
                        library::sql_integer(now_seconds())?
                    ],
                )
                .map_err(sql_error)?;
            Ok(json!({"model_id": id, "joined": true}))
        })
    }

    pub fn library_collection_outfit(
        &self,
        actor: Actor,
        id: &str,
        input: &OutfitInput,
    ) -> Result<Value> {
        super::super::resource::operation_id(id)?;
        self.library_transaction(|transaction| {
            if input.in_outfit {
                let model = model(transaction, id, actor, false)?;
                // Whole avatars are equipped; accessories and outfit documents only join the outfit.
                let action = if model.summary.resource_kind == crate::service::outfit::ResourceKind::Avatar {
                    "equip"
                } else {
                    "preview"
                };
                require_action(&model, action)?;
            }
            let changed = transaction.execute(
                "UPDATE library_collection SET in_outfit = ?3 WHERE player_uuid = ?1 AND model_id = ?2",
                params![actor.uuid.to_string(), id, input.in_outfit],
            ).map_err(sql_error)?;
            if changed == 0 {
                return Err(ErrorCode::NotFound);
            }
            Ok(json!({"model_id": id, "in_outfit": input.in_outfit}))
        })
    }
}
