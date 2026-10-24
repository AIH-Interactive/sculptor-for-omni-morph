//! Transactional configuration publication and the existing source-bound base projection.
use super::*;
use crate::service::{
    library::sql_error,
    library_policy::Actor,
    outfit::{AccessoryMetadata, OutfitConfig, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyInput {
    pub operation_id: Uuid,
    pub expected_revision: u64,
    pub configuration: OutfitConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Applied {
    pub configuration: OutfitConfig,
    pub equipment_revision: u64,
}

#[derive(Debug)]
pub(crate) struct ApplyOutcome {
    pub applied: Applied,
    pub changed: bool,
}

struct BaseSource {
    owner: Uuid,
    asset: String,
    revision: u64,
    model: Option<Uuid>,
    model_revision: u64,
    alias_bound: bool,
}

enum ValidatedResource {
    Base(BaseSource),
    Accessory(AccessoryMetadata),
}

fn current_revision(database: &Connection, owner: Uuid) -> Result<u64> {
    let revision: Option<i64> = database
        .query_row(
            "SELECT revision FROM outfit_configurations WHERE owner_uuid = ?1",
            [owner.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    u64::try_from(revision.unwrap_or(0)).map_err(|_| ErrorCode::Internal)
}

fn validate_resource(
    database: &Connection,
    actor: Actor,
    origin: &str,
    reference: &ResourceRef,
    kind: ResourceKind,
    limits: &OutfitLimits,
) -> Result<ValidatedResource> {
    let source = outfit_sources::resolve(database, actor, origin, reference, kind, limits)?;
    if kind == ResourceKind::Accessory {
        return source
            .metadata
            .map(ValidatedResource::Accessory)
            .ok_or(ErrorCode::Conflict);
    }
    let descriptor = source.descriptor;
    let owner = Uuid::parse_str(&descriptor.owner).map_err(|_| ErrorCode::Internal)?;
    let ResourceRef::Cloud {
        resource_id,
        revision,
        ..
    } = reference
    else {
        return Ok(ValidatedResource::Base(BaseSource {
            owner,
            asset: descriptor.asset_id,
            revision: descriptor.revision,
            model: None,
            model_revision: 0,
            alias_bound: false,
        }));
    };
    let alias = format!("lib_{}_{}", resource_id.simple(), revision);
    let alias_bound = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1
         AND asset_id = ?2 AND revision = 1 AND source_model_id = ?3 AND source_revision = ?4)",
            params![
                actor.uuid.to_string(),
                alias,
                resource_id.to_string(),
                library::sql_integer(*revision)?
            ],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(ValidatedResource::Base(BaseSource {
        owner,
        asset: descriptor.asset_id,
        revision: descriptor.revision,
        model: Some(*resource_id),
        model_revision: *revision,
        alias_bound,
    }))
}

fn validate_dependencies(
    database: &Connection,
    actor: Actor,
    origin: &str,
    configuration: &OutfitConfig,
    limits: &OutfitLimits,
) -> Result<Option<BaseSource>> {
    let base = configuration
        .base
        .as_ref()
        .map(|reference| {
            validate_resource(
                database,
                actor,
                origin,
                reference,
                ResourceKind::Avatar,
                limits,
            )
        })
        .transpose()?;
    let base = match base {
        Some(ValidatedResource::Base(source)) => Some(source),
        None => None,
        Some(ValidatedResource::Accessory(_)) => return Err(ErrorCode::Internal),
    };
    let mut checked = std::collections::HashMap::new();
    let mut declarations = BTreeMap::new();
    for instance in &configuration.instances {
        let identity =
            serde_json::to_string(&instance.resource).map_err(|_| ErrorCode::InvalidMessage)?;
        if !checked.contains_key(&identity) {
            let resource = validate_resource(
                database,
                actor,
                origin,
                &instance.resource,
                ResourceKind::Accessory,
                limits,
            )?;
            let ValidatedResource::Accessory(metadata) = resource else {
                return Err(ErrorCode::Internal);
            };
            checked.insert(identity.clone(), metadata);
        }
        declarations.insert(instance.instance_id, checked[&identity].clone());
    }
    crate::service::outfit_capabilities::validate(configuration, &declarations, limits)?;
    Ok(base)
}

fn project_base(manifest: &mut Manifest, owner: Uuid, base: Option<BaseSource>) -> Result<u64> {
    let descriptor = if let Some(source) = base {
        let stored = manifest
            .owners
            .get(&source.owner)
            .and_then(|account| account.assets.get(&source.asset))
            .and_then(|history| history.get(&source.revision))
            .filter(|stored| !stored.tombstone)
            .ok_or(ErrorCode::NotFound)?;
        let mut descriptor = AvatarDescriptor::decode(stored.descriptor.as_slice())
            .map_err(|_| ErrorCode::Internal)?;
        if let Some(model) = source.model {
            descriptor.owner = owner.to_string();
            descriptor.asset_id = format!("lib_{}_{}", model.simple(), source.model_revision);
            descriptor.revision = 1;
            let encoded = descriptor.encode_to_vec();
            let account = manifest.owners.entry(owner).or_default();
            if let Some(history) = account.assets.get(&descriptor.asset_id) {
                let stored = history.get(&1).ok_or(ErrorCode::Conflict)?;
                if !source.alias_bound || stored.tombstone || stored.descriptor != encoded {
                    return Err(ErrorCode::Conflict);
                }
            } else {
                if account.assets.len() >= 1024 {
                    return Err(ErrorCode::LimitExceeded);
                }
                account.assets.insert(
                    descriptor.asset_id.clone(),
                    BTreeMap::from([(
                        1,
                        StoredAvatar {
                            descriptor: encoded,
                            tombstone: false,
                        },
                    )]),
                );
            }
        }
        Some(descriptor)
    } else {
        None
    };
    let account = manifest.owners.entry(owner).or_default();
    let reference = descriptor.as_ref().map(asset_ref);
    let compatibility = match descriptor.as_ref().map(|descriptor| descriptor.model_type) {
        Some(1) => reference.clone(),
        Some(2 | 3) => account.compatibility.clone(),
        None => None,
        _ => return Err(ErrorCode::UnsupportedCapability),
    };
    select(account, reference, compatibility);
    Ok(account.revision)
}

impl Storage {
    pub(crate) fn outfit_apply(
        &self,
        actor: Actor,
        origin: &str,
        input: &ApplyInput,
        limits: &OutfitLimits,
    ) -> Result<Applied> {
        self.outfit_apply_with_status(actor, origin, input, limits)
            .map(|outcome| outcome.applied)
    }

    /// Report fresh publication separately from the immutable idempotent receipt.
    pub(crate) fn outfit_apply_with_status(
        &self,
        actor: Actor,
        origin: &str,
        input: &ApplyInput,
        limits: &OutfitLimits,
    ) -> Result<ApplyOutcome> {
        input.configuration.validate_with_limits(limits)?;
        if input.configuration.revision != input.expected_revision
            || input.expected_revision >= i64::MAX as u64
        {
            return Err(ErrorCode::Conflict);
        }
        let fingerprint = sha256(
            &serde_json::to_vec(&("outfit-apply", origin, input))
                .map_err(|_| ErrorCode::InvalidMessage)?,
        );
        let _publication = self.lock_blob_files()?;
        let prepare = |manifest: &mut Manifest| {
            let (base, prior) = self.library_read(|database| {
                let base = validate_dependencies(database, actor, origin, &input.configuration, limits)?;
                let prior: Option<(Vec<u8>, Vec<u8>)> = database.query_row(
                    "SELECT fingerprint, result FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
                    params![actor.uuid.to_string(), input.operation_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional().map_err(sql_error)?;
                if let Some((saved, _)) = &prior {
                    if saved != &fingerprint {
                        return Err(ErrorCode::Conflict);
                    }
                } else if current_revision(database, actor.uuid)? != input.expected_revision {
                    return Err(ErrorCode::Conflict);
                }
                Ok((base, prior))
            })?;
            if let Some((_, bytes)) = prior {
                let applied: Applied =
                    serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)?;
                return Ok((applied, false));
            }
            let equipment_revision = project_base(manifest, actor.uuid, base)?;
            let mut configuration = input.configuration.clone();
            configuration.revision = input.expected_revision + 1;
            configuration.validate_with_limits(limits)?;
            Ok((
                Applied {
                    configuration,
                    equipment_revision,
                },
                true,
            ))
        };
        let publish = |transaction: &rusqlite::Transaction<'_>, result: &(Applied, bool)| {
            let applied = &result.0;
            let base =
                validate_dependencies(transaction, actor, origin, &input.configuration, limits)?;
            let save = |transaction: &rusqlite::Transaction<'_>| {
                if current_revision(transaction, actor.uuid)? != input.expected_revision {
                    return Err(ErrorCode::Conflict);
                }
                if let Some(source) = base {
                    let (asset, revision) = if let Some(model) = source.model {
                        let alias = format!("lib_{}_{}", model.simple(), source.model_revision);
                        transaction.execute(
                            "INSERT OR IGNORE INTO library_resource_bindings VALUES (?1, ?2, 1, ?3, ?4)",
                            params![
                                actor.uuid.to_string(), alias, model.to_string(),
                                library::sql_integer(source.model_revision)?
                            ],
                        ).map_err(sql_error)?;
                        transaction.execute(
                            "INSERT INTO library_equipment_settings VALUES (?1, ?2, 1, ?3)
                             ON CONFLICT(resource_owner, asset_id, revision)
                             DO UPDATE SET show_private_equipment = excluded.show_private_equipment",
                            params![actor.uuid.to_string(), alias, applied.configuration.show_equipment],
                        ).map_err(sql_error)?;
                        (alias, 1)
                    } else {
                        (source.asset, source.revision)
                    };
                    transaction
                        .execute(
                            "INSERT INTO library_wardrobe_settings VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(owner_uuid, asset_id, asset_revision)
                         DO UPDATE SET show_equipment = excluded.show_equipment",
                            params![
                                actor.uuid.to_string(),
                                asset,
                                library::sql_integer(revision)?,
                                applied.configuration.show_equipment
                            ],
                        )
                        .map_err(sql_error)?;
                }
                let json = serde_json::to_string(&applied.configuration)
                    .map_err(|_| ErrorCode::Internal)?;
                transaction
                    .execute(
                        "INSERT INTO outfit_configurations VALUES (?1, ?2, ?3)
                     ON CONFLICT(owner_uuid) DO UPDATE SET revision = excluded.revision,
                     configuration_json = excluded.configuration_json",
                        params![
                            actor.uuid.to_string(),
                            library::sql_integer(applied.configuration.revision)?,
                            json
                        ],
                    )
                    .map_err(sql_error)?;
                serde_json::to_vec(applied).map_err(|_| ErrorCode::Internal)
            };
            let bytes = library::operation(
                transaction,
                actor.uuid,
                input.operation_id,
                &fingerprint,
                now_seconds(),
                save,
            )?;
            if serde_json::from_slice::<Applied>(&bytes).map_err(|_| ErrorCode::Internal)?
                != *applied
            {
                return Err(ErrorCode::Conflict);
            }
            Ok(())
        };
        self.transaction_with_library_result(prepare, publish)
            .map(|(applied, changed)| ApplyOutcome { applied, changed })
    }
}
