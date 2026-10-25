//! Fixed native sources shared by Apply and observer projection. No catalog download grant.
use super::*;
use crate::service::{
    library::{model, require_action, sql_error},
    library_policy::{Actor, Policy},
    outfit::{AccessoryMetadata, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) struct Source {
    pub descriptor: AvatarDescriptor,
    pub metadata: Option<AccessoryMetadata>,
    pub policy: Option<Policy>,
}

fn native(
    database: &Connection,
    owner: Uuid,
    asset: &str,
    revision: u64,
) -> Result<AvatarDescriptor> {
    let bytes: Option<Vec<u8>> = database
        .query_row(
            "SELECT descriptor FROM avatars WHERE owner_uuid = ?1 AND asset_id = ?2
             AND revision = ?3 AND tombstone = 0",
            params![owner.to_string(), asset, library::sql_integer(revision)?],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    AvatarDescriptor::decode(bytes.ok_or(ErrorCode::NotFound)?.as_slice())
        .map_err(|_| ErrorCode::Internal)
}

/// Resolve as the wearer first. An observer's grant cannot revive a revoked wearer.
pub(super) fn resolve(
    database: &Connection,
    actor: Actor,
    origin: &str,
    reference: &ResourceRef,
    kind: ResourceKind,
    limits: &OutfitLimits,
) -> Result<Source> {
    reference.validate()?;
    if matches!(reference, ResourceRef::Local { .. }) {
        let binding =
            outfit_resources::resolve_local(database, actor.uuid, reference, kind, limits)?;
        return Ok(Source {
            descriptor: native(
                database,
                actor.uuid,
                &binding.asset_id,
                binding.asset_revision,
            )?,
            metadata: binding.accessory_metadata,
            policy: None,
        });
    }
    let ResourceRef::Cloud {
        origin: source_origin,
        resource_id,
        revision,
        content_hash,
        format,
    } = reference
    else {
        return Err(ErrorCode::InvalidMessage);
    };
    if origin != source_origin {
        return Err(ErrorCode::UnsupportedCapability);
    }
    let item = model(database, &resource_id.to_string(), actor, false)?;
    require_action(
        &item,
        if kind == ResourceKind::Avatar {
            "equip"
        } else {
            "preview"
        },
    )?;
    if item.summary.resource_kind != kind
        || item.summary.revision != library::sql_integer(*revision)?
        || item.summary.model_type.as_deref() != Some(format.as_str())
    {
        return Err(ErrorCode::Conflict);
    }
    let row: Option<(String, String, String, i64)> = database
        .query_row(
            "SELECT raw_sha256, resource_owner, asset_id, resource_revision FROM library_revisions
             WHERE model_id = ?1 AND revision = ?2",
            params![resource_id.to_string(), library::sql_integer(*revision)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let (hash, owner, asset, source_revision) = row.ok_or(ErrorCode::NotFound)?;
    if hash != *content_hash {
        return Err(ErrorCode::Conflict);
    }
    let owner = Uuid::parse_str(&owner).map_err(|_| ErrorCode::Internal)?;
    let source_revision = u64::try_from(source_revision).map_err(|_| ErrorCode::Internal)?;
    let descriptor = native(database, owner, &asset, source_revision)?;
    if faster_hex::hex_string(&descriptor.raw_sha256) != hash {
        return Err(ErrorCode::Conflict);
    }
    if library_wardrobe::banned(database, owner, &asset, source_revision)? {
        return Err(ErrorCode::Forbidden);
    }
    let metadata = if kind == ResourceKind::Accessory {
        let json: Option<String> = database
            .query_row(
                "SELECT metadata_json FROM library_accessory_metadata
                 WHERE model_id = ?1 AND revision = ?2",
                params![resource_id.to_string(), library::sql_integer(*revision)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        let metadata: AccessoryMetadata = serde_json::from_str(&json.ok_or(ErrorCode::Conflict)?)
            .map_err(|_| ErrorCode::Internal)?;
        metadata.validate_with_limits(limits)?;
        Some(metadata)
    } else {
        None
    };
    Ok(Source {
        descriptor,
        metadata,
        policy: Some(item.policy),
    })
}

impl Source {
    /// Current equipment may be rendered without making the private catalog downloadable.
    pub(super) fn can_render(
        &self,
        database: &Connection,
        reference: &ResourceRef,
        wearer: Uuid,
        observer: Uuid,
        show_equipment: bool,
    ) -> Result<bool> {
        if wearer != observer
            && self.metadata.is_some()
            && !crate::service::outfit::asset_distribution_enabled()
        {
            return Ok(false);
        }
        if wearer != observer && !show_equipment {
            return Ok(false);
        }
        let Some(policy) = self.policy else {
            return Ok(true);
        };
        let ResourceRef::Cloud { resource_id, .. } = reference else {
            return Err(ErrorCode::Internal);
        };
        let granted: bool = database
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_grants WHERE model_id = ?1 AND player_uuid = ?2)",
                params![resource_id.to_string(), observer.to_string()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        Ok(policy.can_render_equipment(wearer, observer, granted, show_equipment))
    }
}
