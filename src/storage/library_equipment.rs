//! Atomic source aliases and the player's current equipment intent.
use super::*;
use crate::service::{
    library::{model, require_action, sql_error},
    library_policy::{Actor, Moderation, Visibility},
};
use rusqlite::{params, OptionalExtension};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LibraryEquip {
    pub operation_id: String,
    pub model_revision: u64,
    pub expected_equipment_revision: u64,
    #[serde(default)]
    pub show_private_equipment: bool,
    /// General equipment visibility; absent preserves legacy public/default-private behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_equipment: Option<bool>,
}

impl Storage {
    pub(crate) fn library_equipment_session(&self, owner: Uuid) -> Result<serde_json::Value> {
        self.library_read(|database| {
            let state = database.query_row(
                "SELECT o.revision, b.source_model_id, b.source_revision,
                 CASE WHEN m.moderation_status = 'ACTIVE' AND m.visibility = 'PRIVATE'
                 THEN COALESCE(s.show_private_equipment, 0) ELSE 0 END,
                 CASE WHEN m.moderation_status != 'ACTIVE' THEN 0
                      WHEN m.visibility = 'PRIVATE' THEN COALESCE(s.show_private_equipment, 0)
                      WHEN m.visibility = 'PUBLIC' THEN COALESCE(w.show_equipment, 1) ELSE 0 END
                 FROM owners o LEFT JOIN equipment e ON e.owner_uuid = o.uuid AND e.slot = 'primary'
                 LEFT JOIN library_resource_bindings b ON b.resource_owner = e.owner_uuid
                   AND b.asset_id = e.asset_id AND b.revision = e.revision
                 LEFT JOIN library_equipment_settings s ON s.resource_owner = e.owner_uuid
                   AND s.asset_id = e.asset_id AND s.revision = e.revision
                 LEFT JOIN library_effective_models m ON m.model_id = b.source_model_id
                 LEFT JOIN library_wardrobe_settings w ON w.owner_uuid = e.owner_uuid
                   AND w.asset_id = e.asset_id AND w.asset_revision = e.revision
                 WHERE o.uuid = ?1", [owner.to_string()], |row| {
                    Ok(serde_json::json!({
                        "equipment_revision": row.get::<_, i64>(0)?,
                        "equipment_model_id": row.get::<_, Option<String>>(1)?,
                        "equipment_model_revision": row.get::<_, Option<i64>>(2)?,
                        "show_private_equipment": row.get::<_, bool>(3)?,
                        "show_equipment": row.get::<_, bool>(4)?,
                    }))
                },
            ).optional().map_err(sql_error)?;
            Ok(state.unwrap_or_else(|| serde_json::json!({
                "equipment_revision": 0, "equipment_model_id": null,
                "equipment_model_revision": null, "show_private_equipment": false, "show_equipment": false,
            })))
        })
    }

    pub(crate) fn library_equip(
        &self,
        actor: Actor,
        id: &str,
        input: &LibraryEquip,
        origin: &str,
        limits: &crate::service::outfit_limits::OutfitLimits,
    ) -> Result<EquippedState> {
        let model_uuid = crate::service::resource::operation_id(id)?;
        let operation = crate::service::resource::operation_id(&input.operation_id)?;
        let fingerprint = sha256(
            &serde_json::to_vec(&("library-equip", id, input)).map_err(|_| ErrorCode::Internal)?,
        );
        let alias_id = format!("lib_{}_{}", model_uuid.simple(), input.model_revision);
        let (source, prior_result, alias_bound) = self.library_read(|database| {
            let item = model(database, id, actor, false)?;
            require_action(&item, "equip")?;
            if item.summary.revision != library::sql_integer(input.model_revision)? {
                return Err(ErrorCode::Conflict);
            }
            if item.policy.visibility == Visibility::Private
                && input.show_equipment.unwrap_or(input.show_private_equipment)
                && (actor.uuid != item.policy.uploader
                    || item.policy.moderation != Moderation::Active)
            {
                return Err(ErrorCode::Forbidden);
            }
            let source: (String, String, i64) = database
                .query_row(
                    "SELECT resource_owner, asset_id, resource_revision FROM library_revisions
                 WHERE model_id = ?1 AND revision = ?2",
                    params![id, library::sql_integer(input.model_revision)?],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(sql_error)?;
            let prior: Option<(Vec<u8>, Vec<u8>)> = database
                .query_row(
                    "SELECT fingerprint, result FROM library_operations
                 WHERE actor_uuid = ?1 AND operation_id = ?2",
                    params![actor.uuid.to_string(), operation.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql_error)?;
            let prior_result = match prior {
                Some((saved, result)) if saved == fingerprint => Some(result),
                Some(_) => return Err(ErrorCode::Conflict),
                None => None,
            };
            let alias_bound: bool = database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_resource_bindings
                 WHERE resource_owner = ?1 AND asset_id = ?2 AND revision = 1
                   AND source_model_id = ?3 AND source_revision = ?4)",
                    params![
                        actor.uuid.to_string(),
                        alias_id,
                        id,
                        library::sql_integer(input.model_revision)?
                    ],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            Ok((source, prior_result, alias_bound))
        })?;
        if let Some(result) = prior_result {
            return EquippedState::decode(result.as_slice()).map_err(|_| ErrorCode::Internal);
        }
        let source_owner = Uuid::parse_str(&source.0).map_err(|_| ErrorCode::Internal)?;
        let source_revision = u64::try_from(source.2).map_err(|_| ErrorCode::Internal)?;
        let mut descriptor = self.avatar(source_owner, &source.1, source_revision)?;
        descriptor.owner = actor.uuid.to_string();
        descriptor.asset_id = alias_id.clone();
        descriptor.revision = 1;
        let encoded = descriptor.encode_to_vec();
        self.transaction_with_library_result(
            |manifest| {
                if let Some(result) = replay::<EquippedState>(
                    manifest,
                    actor.uuid,
                    &input.operation_id,
                    &fingerprint,
                )? {
                    return Ok(result);
                }
                let account = manifest.owners.entry(actor.uuid).or_default();
                if account.revision != input.expected_equipment_revision {
                    return Err(ErrorCode::Conflict);
                }
                if let Some(history) = account.assets.get(&alias_id) {
                    let stored = history.get(&1).ok_or(ErrorCode::Conflict)?;
                    if !alias_bound || stored.tombstone || stored.descriptor != encoded {
                        return Err(ErrorCode::Conflict);
                    }
                } else {
                    if account.assets.len() >= 1024 {
                        return Err(ErrorCode::LimitExceeded);
                    }
                    account.assets.insert(
                        alias_id.clone(),
                        BTreeMap::from([(
                            1,
                            StoredAvatar {
                                descriptor: encoded.clone(),
                                tombstone: false,
                            },
                        )]),
                    );
                }
                let reference = Some(asset_ref(&descriptor));
                let compatibility = match descriptor.model_type {
                    1 => reference.clone(),
                    2 | 3 => account.compatibility.clone(),
                    _ => return Err(ErrorCode::UnsupportedCapability),
                };
                let previous_revision = account.revision;
                select(account, reference, compatibility);
                // An intent change on the same alias must invalidate prior observers/YSM state.
                if account.revision == previous_revision {
                    account.revision = account.revision.checked_add(1).ok_or(ErrorCode::Internal)?;
                }
                let result = equipment(actor.uuid, Some(account))?;
                remember(
                    manifest,
                    actor.uuid,
                    &input.operation_id,
                    &fingerprint,
                    &result,
                )?;
                Ok(result)
            },
            |transaction, result| {
                // Final policy check and publication share the equipment SQLite transaction.
                let item = model(transaction, id, actor, false)?;
                require_action(&item, "equip")?;
                if item.summary.revision != library::sql_integer(input.model_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                if item.policy.visibility == Visibility::Private
                    && input.show_equipment.unwrap_or(input.show_private_equipment)
                    && (actor.uuid != item.policy.uploader
                        || item.policy.moderation != Moderation::Active)
                {
                    return Err(ErrorCode::Forbidden);
                }
                let saved = library::operation(
                    transaction,
                    actor.uuid,
                    operation,
                    &fingerprint,
                    now_seconds(),
                    |transaction| {
                        transaction.execute(
                            "INSERT OR IGNORE INTO library_resource_bindings VALUES (?1, ?2, 1, ?3, ?4)",
                            params![
                                actor.uuid.to_string(),
                                alias_id,
                                id,
                                library::sql_integer(input.model_revision)?
                            ],
                        ).map_err(sql_error)?;
                        transaction.execute(
                            "INSERT INTO library_equipment_settings VALUES (?1, ?2, 1, ?3)
                             ON CONFLICT(resource_owner, asset_id, revision)
                             DO UPDATE SET show_private_equipment = excluded.show_private_equipment",
                            params![actor.uuid.to_string(), alias_id, input.show_equipment.unwrap_or(input.show_private_equipment)],
                        ).map_err(sql_error)?;
                        if let Some(showing) = input.show_equipment {
                            transaction.execute(
                                "INSERT INTO library_wardrobe_settings VALUES (?1, ?2, 1, ?3)
                                 ON CONFLICT(owner_uuid, asset_id, asset_revision)
                                 DO UPDATE SET show_equipment = excluded.show_equipment",
                                params![actor.uuid.to_string(), alias_id, showing],
                            ).map_err(sql_error)?;
                        }
                        let showing = if item.policy.visibility == Visibility::Private {
                            input.show_equipment.unwrap_or(input.show_private_equipment)
                        } else {
                            transaction.query_row(
                                "SELECT COALESCE((SELECT show_equipment FROM library_wardrobe_settings
                                 WHERE owner_uuid = ?1 AND asset_id = ?2 AND asset_revision = 1), 1)",
                                params![actor.uuid.to_string(), alias_id], |row| row.get(0),
                            ).map_err(sql_error)?
                        };
                        let reference = crate::service::outfit::ResourceRef::Cloud {
                            origin: origin.into(), resource_id: model_uuid, revision: input.model_revision,
                            content_hash: faster_hex::hex_string(&descriptor.raw_sha256),
                            format: item.summary.model_type.clone().ok_or(ErrorCode::Internal)?,
                        };
                        super::outfit_legacy::replace_configuration(
                            transaction, actor.uuid, Some(reference), showing, limits,
                        )?;
                        Ok(result.encode_to_vec())
                    },
                )?;
                if saved != result.encode_to_vec() {
                    return Err(ErrorCode::Conflict);
                }
                Ok(())
            },
        )
    }

}
