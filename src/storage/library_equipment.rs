//! Atomic source aliases and the player's current equipment intent.
use super::*;
use crate::service::{
    library::{model, require_action, sql_error},
    library_policy::{Actor, Moderation},
};
use rusqlite::{OptionalExtension, params};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LibraryEquip {
    pub operation_id: String,
    pub model_revision: u64,
    pub expected_equipment_revision: u64,
    #[serde(default)]
    pub show_private_equipment: bool,
}

impl Storage {
    pub(crate) fn library_equip(
        &self,
        actor: Actor,
        id: &str,
        input: &LibraryEquip,
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
            if input.show_private_equipment
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
                if input.show_private_equipment
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
                            params![actor.uuid.to_string(), alias_id, input.show_private_equipment],
                        ).map_err(sql_error)?;
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
